//! `P-Access-Network-Info` for the LTE registration path.
//!
//! Split out of `volte::mod` (which otherwise mixes attach/detach, the
//! `ImsTransport` impl, and this) so the serving-cell parsing and the PANI
//! header it feeds live on their own. TS 24.229 §7.2A.4.

use crate::modules::at_commander::AtCommander;
use std::path::Path;

/// Builds the `P-Access-Network-Info` value for an LTE registration.
///
/// TS 24.229 §7.2A.4: over E-UTRAN the UE reports the access type and the cell
/// it is camped on. Sending the VoWiFi path's `3GPP-WLAN` here would describe
/// the wrong access leg entirely, which is a plausible reason for a P-CSCF to
/// reject a registration that is otherwise perfectly reachable.
///
/// The cell identity is `<MCC><MNC><TAC><ECI>`, all hex for the last two.
pub fn access_network_info(serving_cell: Option<&str>) -> String {
    match serving_cell.and_then(parse_serving_cell) {
        Some(cell) => format!(
            "3GPP-E-UTRAN-FDD; utran-cell-id-3gpp={}{}{}{}",
            cell.mcc, cell.mnc, cell.tac, cell.eci
        ),
        // A bare access-type token is still valid and far better than
        // claiming WLAN; networks that do not police the cell id accept it.
        None => "3GPP-E-UTRAN-FDD".to_string(),
    }
}

/// The identity fields of an LTE serving cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingCell {
    pub mcc: String,
    pub mnc: String,
    /// Tracking Area Code, hex.
    pub tac: String,
    /// E-UTRAN Cell Identifier, hex (28 bits, 7 hex digits).
    pub eci: String,
}

/// Parses `AT+QENG="servingcell"` for LTE.
///
/// ```text
/// +QENG: "servingcell","NOCONN","LTE","FDD",404,43,6BD6814,247,1357,3,3,3,D55E,...
///                                           MCC MNC  ECI    PCID EARFCN     TAC
/// ```
pub fn parse_serving_cell(line: &str) -> Option<ServingCell> {
    let payload = line.trim().strip_prefix("+QENG:")?;
    let f: Vec<String> = payload
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .collect();
    // Only LTE is handled; a 2G/3G serving cell has a different layout and
    // would not be carrying an IMS PDN anyway.
    if f.len() < 13 || f.get(2).map(String::as_str) != Some("LTE") {
        return None;
    }
    let (mcc, mnc, eci, tac) = (&f[4], &f[5], &f[6], &f[12]);
    if mcc.is_empty() || mnc.is_empty() || eci.is_empty() || tac.is_empty() {
        return None;
    }
    Some(ServingCell {
        mcc: mcc.clone(),
        mnc: mnc.clone(),
        tac: tac.to_uppercase(),
        eci: eci.to_uppercase(),
    })
}

/// Reads the serving cell from the modem, for `access_network_info`.
pub fn read_access_network_info(modem_port: &Path) -> String {
    use crate::modules::at_commander::AtResponse;
    let line = AtCommander::open(modem_port)
        .and_then(|mut at| at.send_command("AT+QENG=\"servingcell\""))
        .ok()
        .and_then(|resp| match resp {
            AtResponse::Ok(lines) => lines.into_iter().find(|l| l.contains("+QENG:")),
            _ => None,
        });
    let value = access_network_info(line.as_deref());
    tracing::info!(access_network_info = %value, "derived P-Access-Network-Info");
    value
}

#[cfg(test)]
mod pani_tests {
    use super::*;

    /// Verbatim from the reference EC200U on Vi India.
    const QENG_LTE: &str = "+QENG: \"servingcell\",\"NOCONN\",\"LTE\",\"FDD\",404,43,6BD6814,247,1357,3,3,3,D55E,-95,-13,-66,41,28";

    #[test]
    fn parses_the_lte_serving_cell() {
        let cell = parse_serving_cell(QENG_LTE).expect("should parse");

        assert_eq!(cell.mcc, "404");
        assert_eq!(cell.mnc, "43");
        assert_eq!(cell.eci, "6BD6814");
        assert_eq!(cell.tac, "D55E");
    }

    #[test]
    fn builds_the_eutran_access_network_info() {
        let v = access_network_info(Some(QENG_LTE));

        assert_eq!(v, "3GPP-E-UTRAN-FDD; utran-cell-id-3gpp=40443D55E6BD6814");
    }

    #[test]
    fn never_claims_wlan_when_the_cell_is_unknown() {
        // Reporting the VoWiFi access leg from an LTE registration would
        // describe the wrong access entirely; a bare access-type token is the
        // correct degradation.
        for input in [None, Some("+CME ERROR: 58"), Some("")] {
            let v = access_network_info(input);
            assert_eq!(v, "3GPP-E-UTRAN-FDD");
            assert!(!v.contains("WLAN"));
        }
    }

    #[test]
    fn ignores_a_non_lte_serving_cell() {
        let gsm = "+QENG: \"servingcell\",\"NOCONN\",\"GSM\",404,43,D55E,6BD6,55,,,,";

        assert_eq!(parse_serving_cell(gsm), None);
    }

    #[test]
    fn rejects_a_truncated_serving_cell_line() {
        assert_eq!(
            parse_serving_cell("+QENG: \"servingcell\",\"NOCONN\",\"LTE\""),
            None
        );
    }
}
