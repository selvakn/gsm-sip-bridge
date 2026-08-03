use std::fmt;

pub type PjStatus = i32;

pub const PJ_SUCCESS: PjStatus = 0;

#[derive(Debug, Clone)]
pub enum PjsipError {
    InitFailed(String),
    TransportCreate(String),
    AccountRegister(String),
    CallMake(String),
    CallHangup(String),
    CallAnswer(String),
    MediaPort(String),
    Status(PjStatus),
}

impl PjsipError {
    pub fn from_status(status: PjStatus, context: &str) -> Self {
        if status == PJ_SUCCESS {
            return PjsipError::Status(status);
        }
        PjsipError::Status(status).with_context(context)
    }

    fn with_context(self, context: &str) -> Self {
        match self {
            PjsipError::Status(s) => PjsipError::InitFailed(format!(
                "{context}: pj_status={s} ({})",
                pj_status_to_str(s)
            )),
            other => other,
        }
    }
}

impl fmt::Display for PjsipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PjsipError::InitFailed(msg) => write!(f, "PJSIP init failed: {msg}"),
            PjsipError::TransportCreate(msg) => write!(f, "transport creation failed: {msg}"),
            PjsipError::AccountRegister(msg) => write!(f, "account registration failed: {msg}"),
            PjsipError::CallMake(msg) => write!(f, "make call failed: {msg}"),
            PjsipError::CallHangup(msg) => write!(f, "hangup failed: {msg}"),
            PjsipError::CallAnswer(msg) => write!(f, "answer failed: {msg}"),
            PjsipError::MediaPort(msg) => write!(f, "media port error: {msg}"),
            PjsipError::Status(s) => write!(f, "PJSIP status: {s}"),
        }
    }
}

impl std::error::Error for PjsipError {}

pub fn pj_status_to_str(status: PjStatus) -> String {
    match status {
        0 => "PJ_SUCCESS".into(),
        70018 => "PJSIP_EBUSY".into(),
        _ => format!("PJ_STATUS_{status}"),
    }
}
