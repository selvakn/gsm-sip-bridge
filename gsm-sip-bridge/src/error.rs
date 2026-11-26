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
    /// A card-reader SIM that the bridge cannot reach right now: reset by
    /// another program, pulled, unpowered, its reader gone, or pcscd itself
    /// down (#233). Built only by the PC/SC transport
    /// (`modules::pcsc_card::pcsc_failure` and `PcscTransport::connect`), so
    /// it never describes a modem line or an answer from the card itself.
    ///
    /// Recognise it with [`BridgeError::is_card_unreachable`], never by its
    /// text. The text keeps the `IMS error:` prefix and adds no word the
    /// existing text classifiers (`map_registration_error`,
    /// `classify_register_failure`) look for, so they classify it exactly as
    /// they did the untyped error it replaces.
    #[error("IMS error: SIM card unreachable: {0}")]
    CardUnreachable(String),
    /// A freshly opened modem AT port that did not complete its sync within
    /// the port's timeout (#237): the modem does not answer, or answers
    /// nothing the sync can prove is its own reply. Built only by
    /// `AtCommander::start_worker` (open and reopen), so it never describes a
    /// command that failed on a channel already in step.
    ///
    /// Recognise it with [`BridgeError::is_port_out_of_step`], never by its
    /// text. The open-retry loops use it to stop retrying: they wait out
    /// another holder of the port, not a modem that does not answer. The
    /// text keeps `discovery error:` and contains `AT command timeout`, so
    /// the `contains("timeout")` classifiers (`map_registration_error`,
    /// `modules::worker`, `gsm_echo`) read it as the timeout it is, and it
    /// does not contain `AT+CSIM failed`, so `sim_recovery` does not count it
    /// as a SIM fault.
    #[error("discovery error: AT command timeout: {0}")]
    PortOutOfStep(String),
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

impl BridgeError {
    /// Whether this is a card-reader SIM the bridge cannot reach right now
    /// (#233), rather than a failure the card or the network reported.
    pub fn is_card_unreachable(&self) -> bool {
        matches!(self, BridgeError::CardUnreachable(_))
    }

    /// Whether this is a modem port that failed its sync on open (#237),
    /// rather than a port another holder has locked.
    pub fn is_port_out_of_step(&self) -> bool {
        matches!(self, BridgeError::PortOutOfStep(_))
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

    #[test]
    fn an_unreachable_card_keeps_the_ims_prefix_and_is_typed() {
        let e = BridgeError::CardUnreachable("x".to_string());
        assert_eq!(e.to_string(), "IMS error: SIM card unreachable: x");
        assert!(e.is_card_unreachable());
        assert!(!BridgeError::Ims("SIM card unreachable: x".to_string()).is_card_unreachable());
    }

    /// FR-008: a port that failed its sync reads as a timeout to every
    /// existing text classifier, and never as a SIM fault.
    #[test]
    fn port_out_of_step_reads_as_a_timeout_to_every_classifier() {
        let e = BridgeError::PortOutOfStep("x".into());
        assert_eq!(e.to_string(), "discovery error: AT command timeout: x");
        assert!(e.is_port_out_of_step());
        assert!(!BridgeError::Discovery("x".into()).is_port_out_of_step());
        assert_eq!(
            crate::ims::session::map_registration_error(&e),
            crate::control::protocol::RegistrationStatus::Timeout
        );
        assert!(!crate::supervise::sim_recovery::has_csim_failure(
            &e.to_string()
        ));
        // `modules/worker.rs` and `bin/gsm_echo.rs` test exactly this.
        assert!(e.to_string().contains("timeout"));
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
