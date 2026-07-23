use crate::endpoint::Endpoint;
use crate::error::PjsipError;

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub sip_server: String,
    pub sip_port: u16,
    pub username: String,
    pub password: String,
    pub display_name: String,
}

pub trait RegistrationListener: Send + Sync {
    fn on_registration_state(&self, is_registered: bool, status_code: u16);
}

pub struct Account {
    #[allow(dead_code)]
    config: AccountConfig,
    // Only read in the stub build; the linked build queries PJSUA's live account
    // status instead (see `is_registered`), so the flag is not authoritative.
    #[allow(dead_code)]
    registered: bool,
    #[cfg(feature = "pjsip-linked")]
    account_id: i32,
}

impl Account {
    pub fn register(
        _endpoint: &Endpoint,
        config: AccountConfig,
        _listener: Option<Box<dyn RegistrationListener>>,
    ) -> Result<Self, PjsipError> {
        #[cfg(feature = "pjsip-linked")]
        {
            use std::ffi::CString;

            unsafe // SAFETY: PJSIP initialized; acc_cfg and pj_str sources live until pjsua_acc_add returns
            {
                let mut acc_cfg: pjsua_sys::pjsua_acc_config = std::mem::zeroed();
                pjsua_sys::pjsua_acc_config_default(&mut acc_cfg);

                let id_str = format!(
                    "\"{}\" <sip:{}@{}:{}>",
                    config.display_name, config.username, config.sip_server, config.sip_port
                );
                let id_cstr = CString::new(id_str).unwrap();
                acc_cfg.id = pjsua_sys::pj_str(id_cstr.as_ptr() as *mut std::os::raw::c_char);

                let reg_uri = format!("sip:{}:{}", config.sip_server, config.sip_port);
                let reg_cstr = CString::new(reg_uri).unwrap();
                acc_cfg.reg_uri = pjsua_sys::pj_str(reg_cstr.as_ptr() as *mut std::os::raw::c_char);

                acc_cfg.cred_count = 1;
                let realm_cstr = CString::new("*").unwrap();
                let user_cstr = CString::new(config.username.clone()).unwrap();
                let pass_cstr = CString::new(config.password.clone()).unwrap();
                let scheme_cstr = CString::new("digest").unwrap();
                acc_cfg.cred_info[0].realm = pjsua_sys::pj_str(realm_cstr.as_ptr() as *mut std::os::raw::c_char);
                acc_cfg.cred_info[0].username = pjsua_sys::pj_str(user_cstr.as_ptr() as *mut std::os::raw::c_char);
                acc_cfg.cred_info[0].data = pjsua_sys::pj_str(pass_cstr.as_ptr() as *mut std::os::raw::c_char);
                acc_cfg.cred_info[0].scheme = pjsua_sys::pj_str(scheme_cstr.as_ptr() as *mut std::os::raw::c_char);
                acc_cfg.cred_info[0].data_type = 0; // plain text

                let mut acc_id: pjsua_sys::pjsua_acc_id = -1;
                let status = pjsua_sys::pjsua_acc_add(&acc_cfg, 1, &mut acc_id);
                if status != crate::error::PJ_SUCCESS {
                    return Err(PjsipError::AccountRegister(format!(
                        "pjsua_acc_add returned {status}"
                    )));
                }

                return Ok(Self {
                    config,
                    registered: true,
                    account_id: acc_id,
                });
            }
        }

        #[cfg(not(feature = "pjsip-linked"))]
        {
            tracing::info!(
                username = %config.username,
                server = %config.sip_server,
                "SIP account registered (stub mode)"
            );
            Ok(Self {
                config,
                registered: true,
            })
        }
    }

    /// Whether the registrar currently has this account registered.
    ///
    /// Queries PJSUA's live account info rather than trusting the fire-and-forget
    /// flag set at [`register`](Self::register) time: `pjsua_acc_add` only
    /// *initiates* the REGISTER, so a `403`/`401` denial from the PBX would
    /// otherwise never be observed. A live query also catches a *later* loss
    /// (a re-registration the PBX rejects mid-run).
    pub fn is_registered(&self) -> bool {
        #[cfg(feature = "pjsip-linked")]
        {
            return (200..300).contains(&self.registration_status());
        }
        #[cfg(not(feature = "pjsip-linked"))]
        {
            self.registered
        }
    }

    /// The status code of the account's most recent REGISTER exchange with the
    /// registrar — `0` before any final response, `200` on success, `4xx/5xx`
    /// on denial. Reads PJSUA's live account info.
    #[cfg(feature = "pjsip-linked")]
    pub fn registration_status(&self) -> i32 {
        unsafe // SAFETY: account_id valid for an added account; info is a plain C struct
        {
            let mut info: pjsua_sys::pjsua_acc_info = std::mem::zeroed();
            if pjsua_sys::pjsua_acc_get_info(self.account_id, &mut info) == crate::error::PJ_SUCCESS
            {
                info.status as i32
            } else {
                0
            }
        }
    }

    /// Blocks until the initial REGISTER gets a **final** response from the
    /// registrar, or `timeout` elapses. `Ok` on a 2xx; `Err` carrying the PBX's
    /// status code on a denial, or on timeout with no final response.
    ///
    /// PJSUA runs the REGISTER on its own worker thread, so polling the live
    /// account status here turns "assumed registered" into "confirmed by the
    /// PBX or reported as denied" — the whole point of the validation.
    #[cfg(feature = "pjsip-linked")]
    pub fn wait_registered(&self, timeout: std::time::Duration) -> Result<(), PjsipError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let code = self.registration_status();
            if (200..300).contains(&code) {
                return Ok(());
            }
            if code >= 300 {
                return Err(PjsipError::AccountRegister(format!(
                    "registrar denied REGISTER with {code}"
                )));
            }
            // code < 200: no final response yet (0 = none, 1xx = provisional).
            if std::time::Instant::now() >= deadline {
                return Err(PjsipError::AccountRegister(format!(
                    "no REGISTER response within {}s (last status {code})",
                    timeout.as_secs()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    /// Stub build: no registrar, so there is nothing to wait on.
    #[cfg(not(feature = "pjsip-linked"))]
    pub fn wait_registered(&self, _timeout: std::time::Duration) -> Result<(), PjsipError> {
        Ok(())
    }

    /// Re-sends a REGISTER for this account. Needed to retry after a denial:
    /// PJSUA may treat a `403` as permanent and stop re-registering on its own,
    /// so a caller backing off must re-trigger it explicitly or the account
    /// would never recover even once the registrar starts accepting again.
    #[cfg(feature = "pjsip-linked")]
    pub fn trigger_registration(&self) {
        unsafe // SAFETY: account_id valid for an added account
        {
            pjsua_sys::pjsua_acc_set_registration(self.account_id, 1);
        }
    }

    /// Stub build: no registrar to poke.
    #[cfg(not(feature = "pjsip-linked"))]
    pub fn trigger_registration(&self) {}

    #[cfg(feature = "pjsip-linked")]
    pub fn account_id(&self) -> i32 {
        self.account_id
    }

    pub fn unregister(&mut self) {
        #[cfg(feature = "pjsip-linked")]
        {
            unsafe // SAFETY: account_id valid for an added account while unregister runs before clear
            {
                pjsua_sys::pjsua_acc_set_registration(self.account_id, 0);
            }
        }
        self.registered = false;
        tracing::info!("SIP account unregistered");
    }
}

impl Drop for Account {
    fn drop(&mut self) {
        if self.registered {
            self.unregister();
        }
    }
}
