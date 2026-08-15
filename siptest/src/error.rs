use thiserror::Error;

#[derive(Error, Debug)]
pub enum SipTestError {
    #[error(transparent)]
    Bridge(#[from] gsm_sip_bridge::error::BridgeError),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("not registered to the bridge")]
    NotRegistered,

    #[error("destination not allowed: {0}")]
    DestinationNotAllowed(String),

    #[error("rate limited, retry after {retry_after_s}s")]
    RateLimited { retry_after_s: u64 },

    #[error("a call is already in progress")]
    CallInProgress,

    #[error("call {0} was evicted (retention cap exceeded)")]
    CallEvicted(String),

    #[error("call {0} not found")]
    CallNotFound(String),

    #[error("invalid destination: {0}")]
    InvalidDestination(String),

    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type SipTestResult<T> = Result<T, SipTestError>;
