use thiserror::Error;

#[derive(Error, Debug)]
pub enum BridgeError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("SIP error: {0}")]
    Sip(String),
    #[error("audio error: {0}")]
    Audio(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("metrics error: {0}")]
    Metrics(String),
    #[error("discovery error: {0}")]
    Discovery(String),
    #[error("SMS error: {0}")]
    Sms(String),
}

impl From<rusqlite::Error> for BridgeError {
    fn from(e: rusqlite::Error) -> Self {
        BridgeError::Store(e.to_string())
    }
}

impl From<toml::de::Error> for BridgeError {
    fn from(e: toml::de::Error) -> Self {
        BridgeError::Config(e.to_string())
    }
}

impl From<std::io::Error> for BridgeError {
    fn from(e: std::io::Error) -> Self {
        BridgeError::Config(e.to_string())
    }
}

pub type BridgeResult<T> = Result<T, BridgeError>;
