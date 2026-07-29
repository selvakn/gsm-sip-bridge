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
    #[error("IMS error: {0}")]
    Ims(String),
    /// An I/O failure that is not attributable to a more specific subsystem.
    ///
    /// This variant exists because `From<std::io::Error>` used to map *every*
    /// I/O error to [`BridgeError::Config`]. A serial port that vanished
    /// mid-call, a socket that refused a connection, and a log file that
    /// could not be written were all reported to the operator as
    /// "configuration error: ..." — pointing at config.toml, which was fine.
    ///
    /// The source is retained rather than stringified, so a caller that cares
    /// can still match on [`std::io::ErrorKind`] instead of parsing a message.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
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

pub type BridgeResult<T> = Result<T, BridgeError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this encodes: an I/O failure must not tell the operator
    /// their configuration is wrong.
    #[test]
    fn an_io_error_is_reported_as_io_not_as_a_configuration_problem() {
        let e: BridgeError =
            std::io::Error::new(std::io::ErrorKind::NotFound, "/dev/ttyUSB2: No such device")
                .into();

        let msg = e.to_string();
        assert!(msg.starts_with("I/O error:"), "got: {msg}");
        assert!(
            !msg.contains("configuration"),
            "an absent serial device is not a config error: {msg}"
        );
    }

    /// The source is preserved, so a caller can still branch on the kind.
    #[test]
    fn the_io_error_kind_survives_the_conversion() {
        let e: BridgeError =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into();

        match e {
            BridgeError::Io(inner) => {
                assert_eq!(inner.kind(), std::io::ErrorKind::PermissionDenied)
            }
            other => panic!("expected Io, got {other:?}"),
        }
    }

    /// A genuine config parse failure still reports as one.
    #[test]
    fn a_toml_parse_failure_is_still_a_configuration_error() {
        let e: BridgeError = toml::from_str::<toml::Value>("not = = toml")
            .unwrap_err()
            .into();
        assert!(e.to_string().starts_with("configuration error:"));
    }
}
