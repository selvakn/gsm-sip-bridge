//! Modem signal quality for the management web UI and the `signal_*` gauges
//! (specs/122-management-webui, research R4/R5).
//!
//! One read is `AT+CSQ` (RSSI) followed by `AT+QENG="servingcell"` (RAT, and
//! RSRP/RSRQ/SINR when on LTE). Both the circuit-switched worker and the
//! VoWiFi/VoLTE modem sweep call [`read_signal`] on a port they already own,
//! so this adds no new AT port user. `volte::pani::parse_serving_cell` parses
//! the same response for cell identity and is deliberately left alone.

use crate::control::protocol::SignalReadingWire;
use crate::modules::at_commander::{AtCommander, AtResponse};

/// One reading. Every measurement is optional: an unknown value is `None`,
/// never `0`, so a missing signal cannot be mistaken for a strong or a weak
/// one (FR-023).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SignalReading {
    /// When it was read, unix seconds.
    pub at_unix: u64,
    /// `-113 + 2·rssi` from `AT+CSQ`; `None` when the modem answers `99`.
    pub rssi_dbm: Option<i32>,
    /// `LTE`, `WCDMA` or `GSM`, from `AT+QENG`.
    pub rat: Option<String>,
    pub rsrp_dbm: Option<i32>,
    pub rsrq_db: Option<i32>,
    pub sinr_db: Option<i32>,
}

/// The LTE-only part of a serving-cell response.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServingCellSignal {
    pub rat: Option<String>,
    pub rsrp_dbm: Option<i32>,
    pub rsrq_db: Option<i32>,
    pub sinr_db: Option<i32>,
}

/// Parses the response lines of `AT+CSQ` (`+CSQ: <rssi>,<ber>`) to dBm.
///
/// `99` is "not known or not detectable" and gives `None`, as does anything
/// outside the 0..=31 range the formula is defined for.
pub fn parse_csq(lines: &[String]) -> Option<i32> {
    let payload = lines
        .iter()
        .find_map(|l| l.trim().strip_prefix("+CSQ:"))?
        .trim();
    let rssi: i32 = payload.split(',').next()?.trim().parse().ok()?;
    (0..=31).contains(&rssi).then(|| -113 + 2 * rssi)
}

/// Parses the response lines of `AT+QENG="servingcell"`.
///
/// ```text
/// +QENG: "servingcell","NOCONN","LTE","FDD",404,43,6BD6814,247,1357,3,3,3,D55E,-95,-13,-66,41,28
///          state            RAT  duplex MCC MNC  ECI    PCID EARFCN band ul dl TAC RSRP RSRQ RSSI SINR
/// ```
///
/// The RAT is returned for LTE, WCDMA and GSM. RSRP/RSRQ/SINR are returned
/// only for the LTE form; `SEARCH`, `LIMSRV` and anything that does not parse
/// leave them `None`. SINR is Quectel's logarithmic value (0..=250 mapping to
/// -20..+30 dB, `-20 + 0.2·v`), converted here to whole dB.
pub fn parse_qeng_servingcell(lines: &[String]) -> ServingCellSignal {
    let Some(payload) = lines
        .iter()
        .find_map(|l| l.trim().strip_prefix("+QENG:"))
        .filter(|p| p.contains("servingcell"))
    else {
        return ServingCellSignal::default();
    };
    let f: Vec<&str> = payload
        .split(',')
        .map(|s| s.trim().trim_matches('"'))
        .collect();
    // `"servingcell","SEARCH"` has no RAT at all.
    let rat = f
        .get(2)
        .copied()
        .filter(|r| matches!(*r, "LTE" | "WCDMA" | "GSM"))
        .map(str::to_string);
    if rat.as_deref() != Some("LTE") || f.len() < 17 {
        return ServingCellSignal {
            rat,
            ..Default::default()
        };
    }
    let int = |i: usize| f.get(i).and_then(|v| v.parse::<i32>().ok());
    ServingCellSignal {
        rat,
        rsrp_dbm: int(13).filter(|v| (-140..=-40).contains(v)),
        rsrq_db: int(14).filter(|v| (-40..=0).contains(v)),
        sinr_db: int(16)
            .filter(|v| (0..=250).contains(v))
            .map(|v| (-20.0 + 0.2 * f64::from(v)).round() as i32),
    }
}

/// Reads RSSI and, where the modem reports one, the serving-cell quality.
///
/// Tolerates either command failing: a timeout on `AT+QENG` still returns the
/// RSSI, and the reverse. A modem that answers neither returns a reading with
/// every field `None`.
pub fn read_signal(at: &mut AtCommander) -> SignalReading {
    read_signal_checked(at).0
}

/// [`read_signal`], also saying whether `AT+CSQ` was answered `OK`. A `99`
/// answer is still an answer, so it counts: the circuit-switched worker uses
/// this as its liveness probe in place of a bare `AT`.
pub fn read_signal_checked(at: &mut AtCommander) -> (SignalReading, bool) {
    let mut reading = SignalReading {
        at_unix: now_unix(),
        ..Default::default()
    };
    let mut csq_answered = false;
    if let Ok(AtResponse::Ok(lines)) = at.send_command("AT+CSQ") {
        csq_answered = true;
        reading.rssi_dbm = parse_csq(&lines);
    }
    if let Ok(AtResponse::Ok(lines)) = at.send_command("AT+QENG=\"servingcell\"") {
        let cell = parse_qeng_servingcell(&lines);
        reading.rat = cell.rat;
        reading.rsrp_dbm = cell.rsrp_dbm;
        reading.rsrq_db = cell.rsrq_db;
        reading.sinr_db = cell.sinr_db;
    }
    (reading, csq_answered)
}

/// Whether a signal poll is due: polling is on (`interval_secs > 0`), the
/// interval has run since the last one, and no call is up. Skipping during a
/// call keeps a poll from queuing in front of call-control AT traffic.
pub fn poll_due(interval_secs: u64, since_last: std::time::Duration, call_active: bool) -> bool {
    interval_secs > 0 && !call_active && since_last.as_secs() >= interval_secs
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl SignalReading {
    /// Whether the reading carries no measurement at all.
    pub fn is_empty(&self) -> bool {
        self.rssi_dbm.is_none()
            && self.rat.is_none()
            && self.rsrp_dbm.is_none()
            && self.rsrq_db.is_none()
            && self.sinr_db.is_none()
    }
}

impl From<&SignalReading> for SignalReadingWire {
    fn from(r: &SignalReading) -> Self {
        Self {
            at_unix: r.at_unix,
            rssi_dbm: r.rssi_dbm,
            rat: r.rat.clone(),
            rsrp_dbm: r.rsrp_dbm,
            rsrq_db: r.rsrq_db,
            sinr_db: r.sinr_db,
        }
    }
}

impl From<SignalReadingWire> for SignalReading {
    fn from(w: SignalReadingWire) -> Self {
        Self {
            at_unix: w.at_unix,
            rssi_dbm: w.rssi_dbm,
            rat: w.rat,
            rsrp_dbm: w.rsrp_dbm,
            rsrq_db: w.rsrq_db,
            sinr_db: w.sinr_db,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Verbatim from the reference EC200U on Vi India (also in `volte::pani`).
    const QENG_LTE: &str = "+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",404,43,6BD6814,247,1357,3,3,3,D55E,-95,-13,-66,41,28";

    #[test]
    fn a_poll_is_due_only_when_on_elapsed_and_idle() {
        use std::time::Duration as D;
        assert!(poll_due(60, D::from_secs(60), false));
        assert!(!poll_due(60, D::from_secs(59), false));
        assert!(
            !poll_due(60, D::from_secs(600), true),
            "never during a call"
        );
        assert!(!poll_due(0, D::from_secs(600), false), "0 disables polling");
    }

    #[test]
    fn csq_converts_to_dbm() {
        assert_eq!(parse_csq(&lines(&["+CSQ: 21,99"])), Some(-71));
        assert_eq!(parse_csq(&lines(&["+CSQ: 0,99"])), Some(-113));
        assert_eq!(parse_csq(&lines(&["+CSQ: 31,0"])), Some(-51));
    }

    #[test]
    fn csq_99_is_unknown_not_zero() {
        assert_eq!(parse_csq(&lines(&["+CSQ: 99,99"])), None);
    }

    #[test]
    fn csq_rejects_garbage() {
        assert_eq!(parse_csq(&lines(&[])), None);
        assert_eq!(parse_csq(&lines(&["+CSQ: abc,1"])), None);
        assert_eq!(parse_csq(&lines(&["+CSQ: 150,1"])), None);
        assert_eq!(parse_csq(&lines(&["+CME ERROR: 10"])), None);
    }

    #[test]
    fn the_lte_form_gives_rat_and_quality() {
        let cell = parse_qeng_servingcell(&lines(&[QENG_LTE]));
        assert_eq!(cell.rat.as_deref(), Some("LTE"));
        assert_eq!(cell.rsrp_dbm, Some(-95));
        assert_eq!(cell.rsrq_db, Some(-13));
        // 41 on Quectel's 0..=250 scale is -20 + 0.2*41 = -11.8 dB.
        assert_eq!(cell.sinr_db, Some(-12));
    }

    #[test]
    fn non_lte_forms_keep_the_rat_but_no_lte_fields() {
        let wcdma = parse_qeng_servingcell(&lines(&[
            "+QENG: \"servingcell\",\"NOCONN\",\"WCDMA\",404,43,D55E,6BD6,10700,1,-70,-8,-90",
        ]));
        assert_eq!(wcdma.rat.as_deref(), Some("WCDMA"));
        assert_eq!(wcdma.rsrp_dbm, None);
        assert_eq!(wcdma.sinr_db, None);

        let gsm = parse_qeng_servingcell(&lines(&[
            "+QENG: \"servingcell\",\"NOCONN\",\"GSM\",404,43,D55E,6BD6,55,,,,",
        ]));
        assert_eq!(gsm.rat.as_deref(), Some("GSM"));
        assert_eq!(gsm.rsrp_dbm, None);
    }

    #[test]
    fn search_and_limsrv_and_failures_give_nothing() {
        for input in [
            "+QENG: \"servingcell\",\"SEARCH\"",
            "+QENG: \"servingcell\",\"LIMSRV\",\"-\",\"-\",0,0",
            "+QENG: \"servingcell\",\"NOCONN\",\"LTE\"",
            "+CME ERROR: 13",
            "",
        ] {
            let cell = parse_qeng_servingcell(&lines(&[input]));
            assert_eq!(cell.rsrp_dbm, None, "{input}");
            assert_eq!(cell.rsrq_db, None, "{input}");
            assert_eq!(cell.sinr_db, None, "{input}");
        }
    }

    #[test]
    fn out_of_range_lte_values_are_dropped() {
        let bad = QENG_LTE.replace(",-95,-13,-66,41,28", ",0,5,-66,999,28");
        let cell = parse_qeng_servingcell(&lines(&[&bad]));
        assert_eq!(cell.rat.as_deref(), Some("LTE"));
        assert_eq!(cell.rsrp_dbm, None);
        assert_eq!(cell.rsrq_db, None);
        assert_eq!(cell.sinr_db, None);
    }

    /// A scripted modem: answers each command in turn.
    fn fake_modem(replies: Vec<&'static str>) -> AtCommander {
        let (client, mut modem) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            for reply in replies {
                match modem.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if modem.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        });
        AtCommander::from_stream(client, Duration::from_millis(300))
    }

    #[test]
    fn read_signal_combines_both_commands() {
        let mut at = fake_modem(vec![
            "\r\n+CSQ: 21,99\r\n\r\nOK\r\n",
            "\r\n+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",404,43,6BD6814,247,1357,3,3,3,D55E,-95,-13,-66,41,28\r\n\r\nOK\r\n",
        ]);
        let r = read_signal(&mut at);
        assert_eq!(r.rssi_dbm, Some(-71));
        assert_eq!(r.rat.as_deref(), Some("LTE"));
        assert_eq!(r.rsrp_dbm, Some(-95));
        assert!(r.at_unix > 0);
    }

    #[test]
    fn read_signal_survives_one_command_failing() {
        let mut at = fake_modem(vec!["\r\n+CSQ: 99,99\r\n\r\nOK\r\n", "\r\nERROR\r\n"]);
        let r = read_signal(&mut at);
        assert_eq!(r.rssi_dbm, None, "99 is unknown");
        assert!(r.is_empty());
    }
}
