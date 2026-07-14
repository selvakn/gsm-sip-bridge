//! `vowifi-plmn`: prints the home network's MCC and MNC (space-separated,
//! MNC zero-padded to 3 digits) derived from the SIM, and exits. Used by
//! `docker/entrypoint.sh` when `vowifi.mcc`/`vowifi.mnc` are left unset in
//! config.toml — the same "ask the binary instead of hand-parsing AT in
//! bash" precedent as `vowifi-imsi`.
//!
//! Derivation: the MCC is always the first 3 IMSI digits (`AT+CIMI`); the
//! MNC is the next 2 *or* 3 digits, and the IMSI alone doesn't say which.
//! The authoritative answer is the SIM's own EF_AD administrative data
//! file (`AT+CRSM`, TS 31.102 §4.2.18); when that's unreadable (legacy 2G
//! SIMs may omit the MNC-length byte), fall back to the registered PLMN
//! from numeric `AT+COPS` — its 5/6-digit operator string makes the length
//! unambiguous, but it describes the *serving* network, which only matches
//! the home network when not roaming.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::AtCommander;
use std::path::Path;
use std::process::ExitCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plmn {
    /// Mobile Country Code, always 3 digits.
    pub mcc: String,
    /// Mobile Network Code, zero-padded to 3 digits — the form every
    /// consumer needs (ePDG FQDN, EAP-AKA NAI realm, IMS realm all use
    /// 3-digit `mnc` labels per TS 23.003, and the config's own
    /// `vowifi.mnc` convention is the padded form, e.g. `"094"`).
    pub mnc: String,
}

/// Split `imsi` into MCC + zero-padded MNC given the home network's MNC
/// digit count. Pure, so the ambiguity resolution (EF_AD vs COPS) and the
/// split itself are testable separately.
pub fn plmn_from_imsi(imsi: &str, mnc_len: u8) -> BridgeResult<Plmn> {
    if !(2..=3).contains(&mnc_len) {
        return Err(BridgeError::Discovery(format!(
            "MNC length must be 2 or 3, got {mnc_len}"
        )));
    }
    let digits_needed = 3 + mnc_len as usize;
    if imsi.len() < digits_needed || !imsi.chars().all(|c| c.is_ascii_digit()) {
        return Err(BridgeError::Discovery(format!(
            "IMSI {imsi:?} too short or non-numeric for a {mnc_len}-digit MNC"
        )));
    }
    Ok(Plmn {
        mcc: imsi[..3].to_string(),
        mnc: format!("{:0>3}", &imsi[3..digits_needed]),
    })
}

/// The testable core: given an already-open transport, derive the home
/// PLMN. Also called by `vowifi-ims-agent` at startup (it builds the IMS
/// realm from MCC/MNC) when the config leaves them unset.
pub fn derive_plmn(at: &mut AtCommander) -> BridgeResult<Plmn> {
    let imsi = at.query_imsi()?;
    let mnc_len = match at.query_mnc_length() {
        Ok(n) => n,
        Err(ef_ad_err) => {
            tracing::warn!(error = %ef_ad_err, "EF_AD unreadable; falling back to AT+COPS");
            let serving = at.query_cops_plmn().map_err(|cops_err| {
                BridgeError::Discovery(format!(
                    "cannot determine MNC length: EF_AD failed ({ef_ad_err}) \
                     and COPS failed ({cops_err}) — set vowifi.mcc/mnc explicitly"
                ))
            })?;
            // The serving PLMN's MNC length only describes the home network
            // when the serving PLMN IS the home network. If it isn't a
            // prefix of the IMSI we're roaming, and the length is a guess.
            if !imsi.starts_with(&serving) {
                tracing::warn!(
                    serving_plmn = %serving,
                    "serving PLMN doesn't match the IMSI (roaming?) — \
                     derived MNC length may be wrong; set vowifi.mcc/mnc explicitly if so"
                );
            }
            (serving.len() - 3) as u8
        }
    };
    plmn_from_imsi(&imsi, mnc_len)
}

pub fn run(modem_port: &Path) -> ExitCode {
    let mut at = match AtCommander::open(modem_port) {
        Ok(at) => at,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match derive_plmn(&mut at) {
        Ok(plmn) => {
            println!("{} {}", plmn.mcc, plmn.mnc);
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
    use std::collections::VecDeque;
    use std::io::{Cursor, Read, Write};
    use std::time::Duration;

    /// Mock stream scripted with one response per command: each write (a
    /// sent command) makes the next response readable. The single-buffer
    /// `MockStream` used elsewhere can't script multi-command flows —
    /// `read_response`'s `BufReader` slurps the whole buffer on the first
    /// command, losing the later responses.
    struct ScriptedStream {
        responses: VecDeque<Vec<u8>>,
        current: Cursor<Vec<u8>>,
    }

    impl ScriptedStream {
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: responses.iter().map(|r| r.as_bytes().to_vec()).collect(),
                current: Cursor::new(Vec::new()),
            }
        }
    }

    impl Read for ScriptedStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.current.read(buf)
        }
    }

    impl Write for ScriptedStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.current = Cursor::new(self.responses.pop_front().unwrap_or_default());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn scripted(responses: &[&str]) -> AtCommander {
        AtCommander::from_stream(ScriptedStream::new(responses), Duration::from_secs(1))
    }

    #[test]
    fn plmn_from_imsi_pads_two_digit_mnc() {
        let plmn = plmn_from_imsi("404940123456789", 2).unwrap();
        assert_eq!(plmn.mcc, "404");
        assert_eq!(plmn.mnc, "094");
    }

    #[test]
    fn plmn_from_imsi_three_digit_mnc() {
        let plmn = plmn_from_imsi("310170123456789", 3).unwrap();
        assert_eq!(plmn.mcc, "310");
        assert_eq!(plmn.mnc, "170");
    }

    #[test]
    fn plmn_from_imsi_rejects_bad_length() {
        assert!(plmn_from_imsi("404940123456789", 1).is_err());
        assert!(plmn_from_imsi("404940123456789", 4).is_err());
    }

    #[test]
    fn plmn_from_imsi_rejects_short_or_non_numeric_imsi() {
        assert!(plmn_from_imsi("4049", 2).is_err());
        assert!(plmn_from_imsi("40494x123456789", 2).is_err());
    }

    #[test]
    fn derive_plmn_uses_ef_ad() {
        let mut at = scripted(&[
            "404940123456789\r\nOK\r\n",           // AT+CIMI
            "+CRSM: 144,0,\"00000002\"\r\nOK\r\n", // AT+CRSM (EF_AD)
        ]);
        let plmn = derive_plmn(&mut at).unwrap();
        assert_eq!(plmn.mcc, "404");
        assert_eq!(plmn.mnc, "094");
    }

    #[test]
    fn derive_plmn_falls_back_to_cops_when_ef_ad_unreadable() {
        let mut at = scripted(&[
            "405840123456789\r\nOK\r\n",         // AT+CIMI
            "ERROR\r\n",                         // AT+CRSM fails
            "+COPS: 0,2,\"405840\",7\r\nOK\r\n", // AT+COPS: 6-digit PLMN
        ]);
        let plmn = derive_plmn(&mut at).unwrap();
        assert_eq!(plmn.mcc, "405");
        assert_eq!(plmn.mnc, "840");
    }

    #[test]
    fn derive_plmn_errors_when_both_sources_fail() {
        let mut at = scripted(&[
            "404940123456789\r\nOK\r\n", // AT+CIMI
            "ERROR\r\n",                 // AT+CRSM fails
            "ERROR\r\n",                 // AT+COPS fails too
        ]);
        let err = derive_plmn(&mut at).unwrap_err().to_string();
        assert!(err.contains("set vowifi.mcc/mnc explicitly"));
    }

    #[test]
    fn derive_plmn_propagates_imsi_failure() {
        let mut at = scripted(&["ERROR\r\n"]);
        assert!(derive_plmn(&mut at).is_err());
    }
}
