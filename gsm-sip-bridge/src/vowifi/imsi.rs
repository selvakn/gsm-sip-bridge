//! `vowifi-imsi`: prints the SIM's IMSI, read via `AT+CIMI` through the
//! existing `AtCommander`. Used by `docker/entrypoint.sh` to render the
//! strongSwan swanctl connection's EAP identity
//! (`0<IMSI>@nai.epc.mnc<MNC>.mcc<MCC>.3gppnetwork.org`, per
//! `swanctl-epdg.conf.template`) — the entrypoint asks the binary rather
//! than hand-parsing `AT+CIMI` in bash, the same precedent as
//! `gsm-sip-bridge config vowifi-enabled`.

use crate::error::BridgeResult;
use crate::modules::at_commander::AtCommander;
use std::path::Path;
use std::process::ExitCode;

/// The testable core: given an already-open transport, read the IMSI.
/// Separated from `run` so tests can supply a scripted `AtCommander`
/// instead of a real serial port (`AtCommander::open` needs real hardware).
fn imsi_from(at: &mut AtCommander) -> BridgeResult<String> {
    at.query_imsi()
}

pub fn run(modem_port: &Path) -> ExitCode {
    let mut at = match AtCommander::open(modem_port) {
        Ok(at) => at,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match imsi_from(&mut at) {
        Ok(imsi) => {
            println!("{imsi}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};
    use std::time::Duration;

    /// Mock stream: reads from a fixed byte buffer, discards writes.
    /// Mirrors `at_commander.rs`'s own test-only `MockStream` — the modem
    /// is hardware unavailable in CI, the same justification already
    /// established at that mock site.
    struct MockStream {
        reader: Cursor<Vec<u8>>,
    }

    impl MockStream {
        fn new(response: &str) -> Self {
            Self {
                reader: Cursor::new(response.as_bytes().to_vec()),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn imsi_from_reads_cimi_response() {
        let mut at = AtCommander::from_stream(
            MockStream::new("404430123456789\r\nOK\r\n"),
            Duration::from_secs(1),
        );
        assert_eq!(imsi_from(&mut at).unwrap(), "404430123456789");
    }

    #[test]
    fn imsi_from_propagates_at_error() {
        let mut at = AtCommander::from_stream(MockStream::new("ERROR\r\n"), Duration::from_secs(1));
        assert!(imsi_from(&mut at).is_err());
    }
}
