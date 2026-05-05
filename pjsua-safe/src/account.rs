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

            unsafe {
                let mut acc_cfg: pjsua_sys::pjsua_acc_config = std::mem::zeroed();
                pjsua_sys::pjsua_acc_config_default(&mut acc_cfg);

                let id_str = format!(
                    "\"{}\" <sip:{}@{}:{}>",
                    config.display_name, config.username, config.sip_server, config.sip_port
                );
                let id_cstr = CString::new(id_str).unwrap();
                acc_cfg.id = pjsua_sys::pj_str(id_cstr.as_ptr() as *mut i8);

                let reg_uri = format!("sip:{}:{}", config.sip_server, config.sip_port);
                let reg_cstr = CString::new(reg_uri).unwrap();
                acc_cfg.reg_uri = pjsua_sys::pj_str(reg_cstr.as_ptr() as *mut i8);

                acc_cfg.cred_count = 1;
                let realm_cstr = CString::new("*").unwrap();
                let user_cstr = CString::new(config.username.clone()).unwrap();
                let pass_cstr = CString::new(config.password.clone()).unwrap();
                let scheme_cstr = CString::new("digest").unwrap();
                acc_cfg.cred_info[0].realm = pjsua_sys::pj_str(realm_cstr.as_ptr() as *mut i8);
                acc_cfg.cred_info[0].username = pjsua_sys::pj_str(user_cstr.as_ptr() as *mut i8);
                acc_cfg.cred_info[0].data = pjsua_sys::pj_str(pass_cstr.as_ptr() as *mut i8);
                acc_cfg.cred_info[0].scheme = pjsua_sys::pj_str(scheme_cstr.as_ptr() as *mut i8);
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

    pub fn is_registered(&self) -> bool {
        self.registered
    }

    #[cfg(feature = "pjsip-linked")]
    pub fn account_id(&self) -> i32 {
        self.account_id
    }

    pub fn unregister(&mut self) {
        #[cfg(feature = "pjsip-linked")]
        {
            // SAFETY: account_id is valid if registered is true
            unsafe {
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
