//! `vowifi-plmn`: prints the home network's MCC and MNC (space-separated,
//! MNC zero-padded to 3 digits) derived from the SIM, and exits. Used by
//! `supervise::orchestrate` when a line's `mcc`/`mnc` (from `[[vowifi.line]]`,
//! or auto-discovery) are left unset — the same "ask the binary instead of
//! hand-parsing AT in bash" precedent as `vowifi-imsi`.
//!
//! Derivation: the MCC is always the first 3 IMSI digits; the MNC is the
//! next 2 *or* 3 digits, and the IMSI alone doesn't say which. The
//! authoritative answer is the SIM's own EF_AD administrative data file
//! (TS 31.102 §4.2.18, byte 4).
//!
//! Both inputs live entirely on the card, so this works over either
//! transport — `--modem <port>` (`AT+CIMI` + `AT+CRSM`) or `--pcsc-imsi
//! <IMSI>` for a `pcsc_reader` line, which reads EF_IMSI and EF_AD straight
//! off the reader named by that IMSI with no modem involved. Only the
//! *fallback* is modem-only: when EF_AD is unreadable (legacy 2G SIMs may
//! omit the MNC-length byte), the modem path falls back to the registered
//! PLMN from numeric `AT+COPS`, whose 5/6-digit operator string makes the
//! length unambiguous. A card reader has no radio and therefore no serving
//! PLMN to ask, so the PC/SC path errors instead and tells the operator to
//! set `mcc`/`mnc` explicitly on that line.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::AtCommander;
use crate::modules::usim::{self, ApduTransport};
use std::path::Path;
use std::process::ExitCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plmn {
    /// Mobile Country Code, always 3 digits.
    pub mcc: String,
    /// Mobile Network Code, zero-padded to 3 digits — the form every
    /// consumer needs (ePDG FQDN, EAP-AKA NAI realm, IMS realm all use
    /// 3-digit `mnc` labels per TS 23.003, and a `[[vowifi.line]].mnc`
    /// override uses the same padded form, e.g. `"094"`).
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

/// Derives the home PLMN from the card alone, over any `ApduTransport`:
/// EF_IMSI for the digits and EF_AD for how many of them are the MNC. No
/// modem, no radio — this is the path a `pcsc_reader` line takes, and the
/// reason `mcc`/`mnc` need not be configured for one.
///
/// There is deliberately no `AT+COPS` fallback here: a card reader has no
/// serving network to ask. A card whose EF_AD omits the MNC-length byte
/// therefore fails, and the error says to pin `mcc`/`mnc` on the line.
pub fn derive_plmn_from_card(card: &mut dyn ApduTransport) -> BridgeResult<Plmn> {
    let aid = usim::discover_usim_aid(card)?;
    usim::select_usim(card, &aid)?;
    let imsi = usim::read_imsi(card)?;
    let mnc_len = usim::read_mnc_length(card).map_err(|e| {
        BridgeError::Discovery(format!(
            "cannot determine MNC length from the card: {e} — a card reader has \
             no serving PLMN to fall back to, so set mcc/mnc explicitly on this line"
        ))
    })?;
    plmn_from_imsi(&imsi, mnc_len)
}

/// The testable core of the modem path: given an already-open transport,
/// derive the home PLMN. Also called by `vowifi-ims-agent` at startup (it
/// builds the IMS realm from MCC/MNC) when the config leaves them unset.
pub fn derive_plmn(at: &mut AtCommander) -> BridgeResult<Plmn> {
    let imsi = at.query_imsi()?;
    let mnc_len = match at.query_mnc_length() {
        Ok(n) => n,
        Err(ef_ad_err) => {
            tracing::warn!(error = %ef_ad_err, "EF_AD unreadable; falling back to AT+COPS");
            let serving = at.query_cops_plmn().map_err(|cops_err| {
                BridgeError::Discovery(format!(
                    "cannot determine MNC length: EF_AD failed ({ef_ad_err}) \
                     and COPS failed ({cops_err}) — set mcc/mnc explicitly on this line"
                ))
            })?;
            // The serving PLMN's MNC length only describes the home network
            // when the serving PLMN IS the home network. If it isn't a
            // prefix of the IMSI we're roaming, and the length is a guess.
            if !imsi.starts_with(&serving) {
                tracing::warn!(
                    serving_plmn = %serving,
                    "serving PLMN doesn't match the IMSI (roaming?) — \
                     derived MNC length may be wrong; set mcc/mnc explicitly on this line if so"
                );
            }
            (serving.len() - 3) as u8
        }
    };
    plmn_from_imsi(&imsi, mnc_len)
}

/// `--modem <port>` derives over the modem's `AT+CSIM`/`AT+CRSM` (with the
/// `AT+COPS` fallback); `--pcsc-imsi <IMSI>` derives over the PC/SC reader
/// holding that IMSI's card. Exactly one is required — the CLI enforces
/// that, so this only has to report the "neither" case defensively.
pub fn run(modem_port: Option<&Path>, pcsc_imsi: Option<&str>) -> ExitCode {
    let result = match (modem_port, pcsc_imsi) {
        (_, Some(imsi)) => crate::modules::pcsc_card::PcscTransport::connect(imsi)
            .and_then(|mut card| card.with_transaction(derive_plmn_from_card)),
        (Some(port), None) => AtCommander::open(port).and_then(|mut at| derive_plmn(&mut at)),
        (None, None) => Err(BridgeError::Discovery(
            "vowifi-plmn needs either --modem <port> or --pcsc-imsi <IMSI>".into(),
        )),
    };
    match result {
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
        assert!(err.contains("set mcc/mnc explicitly on this line"));
    }

    #[test]
    fn derive_plmn_propagates_imsi_failure() {
        let mut at = scripted(&["ERROR\r\n"]);
        assert!(derive_plmn(&mut at).is_err());
    }

    /// Scripts raw APDU responses in order, the `ApduTransport` analogue of
    /// `ScriptedStream` above — lets the card path be tested without a
    /// reader, the same way the modem path is tested without a modem.
    struct QueuedCard(VecDeque<String>);

    impl ApduTransport for QueuedCard {
        fn transmit_apdu(&mut self, _apdu_hex: &str) -> BridgeResult<String> {
            self.0
                .pop_front()
                .ok_or_else(|| BridgeError::Ims("no more scripted responses".into()))
        }
    }

    /// The SELECT/READ RECORD exchange `discover_usim_aid` + `select_usim`
    /// make before any EF can be read, as a real card answers them.
    fn usim_selection_responses() -> Vec<String> {
        vec![
            "9000".to_string(), // SELECT MF (discover_usim_aid)
            "9000".to_string(), // SELECT EF_DIR
            "61184F10A0000000871002FFF605FF89000001FF50045553494D9000".to_string(), // READ RECORD 1
            "9000".to_string(), // SELECT MF (select_usim)
            "9000".to_string(), // SELECT AID
        ]
    }

    #[test]
    fn derive_plmn_from_card_reads_both_files_off_the_card() {
        // The real Vodafone SIM from docs/omnikey-pcsc-vowifi.md (IMSI
        // 404438083996440), with EF_AD declaring a 2-digit MNC — so this is
        // 404/043, the pair that used to have to be hand-written in config.
        let mut responses = usim_selection_responses();
        responses.extend([
            "9000".to_string(),                   // SELECT EF_IMSI
            "0849403408389946049000".to_string(), // READ BINARY EF_IMSI
            "9000".to_string(),                   // SELECT EF_AD
            "000000029000".to_string(),           // READ BINARY EF_AD -> 2-digit MNC
        ]);
        let mut card = QueuedCard(VecDeque::from(responses));
        let plmn = derive_plmn_from_card(&mut card).unwrap();
        assert_eq!(plmn.mcc, "404");
        assert_eq!(plmn.mnc, "043");
    }

    #[test]
    fn derive_plmn_from_card_handles_a_three_digit_mnc() {
        let mut responses = usim_selection_responses();
        responses.extend([
            "9000".to_string(),
            // IMSI 310170123456789: length byte 0x08, then the parity nibble
            // 9 prepended to the digits and the whole run nibble-swapped.
            "0839017110325476989000".to_string(),
            "9000".to_string(),
            "000000039000".to_string(),
        ]);
        let mut card = QueuedCard(VecDeque::from(responses));
        let plmn = derive_plmn_from_card(&mut card).unwrap();
        assert_eq!(plmn.mcc, "310");
        assert_eq!(plmn.mnc, "170");
    }

    #[test]
    fn derive_plmn_from_card_errors_when_ef_ad_is_unreadable() {
        // No AT+COPS to fall back to — a reader has no serving network — so
        // the error has to point the operator at pinning mcc/mnc instead of
        // guessing a length.
        let mut responses = usim_selection_responses();
        responses.extend([
            "9000".to_string(),
            "0849403408389946049000".to_string(),
            "6A82".to_string(), // SELECT EF_AD: file not found
        ]);
        let mut card = QueuedCard(VecDeque::from(responses));
        let err = derive_plmn_from_card(&mut card).unwrap_err().to_string();
        assert!(
            err.contains("set mcc/mnc explicitly on this line"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("no serving PLMN"),
            "the error should say why there is no fallback: {err}"
        );
    }
}
