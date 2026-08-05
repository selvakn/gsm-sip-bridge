//! `pcsc-list`: enumerates every real (non-vpcd) PC/SC reader attached to
//! the host, reads each card's IMSI and home MCC/MNC straight off the SIM
//! (the same EF_IMSI/EF_AD path `pcsc_card::PcscTransport` and
//! `vowifi::plmn::derive_plmn_from_card` use — no modem involved), and looks
//! up the carrier name for that MCC/MNC via mcc-mnc-lookup.com's public API.
//!
//! Exists to answer "which `imsi_override` goes on which `pcsc_reader`
//! line" for a multi-reader deployment (specs/023-omnikey-pcsc-vowifi)
//! without decoding an IMSI by hand off `pySim-read.py`/`opensc-tool`
//! output, which `sample_configs/pcsc-vowifi.toml` used to point operators
//! at.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::pcsc_card::real_reader_candidates;
use crate::modules::usim::{self, ApduTransport};
use pcsc::{Context, Disposition, Protocols, Scope, ShareMode};
use std::collections::HashMap;
use std::process::ExitCode;
use std::time::Duration;

const CARRIER_LOOKUP_URL: &str = "https://mcc-mnc-lookup.com/api/codes/";
const CARRIER_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// One reader's outcome — a card fully read, no card present, or some step
/// along the way failing. Kept as a single struct with optional fields
/// (rather than an enum) because a partial read is common and useful: an
/// unreadable EF_AD (legacy 2G SIMs may omit the MNC-length byte, the same
/// case `vowifi::plmn` handles) shouldn't hide an otherwise-good IMSI, since
/// the IMSI alone is what `imsi_override` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardRow {
    pub reader: String,
    pub status: String,
    pub imsi: Option<String>,
    pub mcc: Option<String>,
    pub mnc: Option<String>,
    pub carrier: Option<String>,
}

#[derive(serde::Deserialize)]
struct LookupResponse {
    results: Vec<LookupEntry>,
}

#[derive(serde::Deserialize)]
struct LookupEntry {
    operator: String,
    country: String,
}

/// Reads EF_IMSI and (best-effort) EF_AD off the currently-reachable card.
/// Errors only when the IMSI itself can't be read — the AID discovery or
/// EF_IMSI read failing means there's nothing useful to report at all,
/// unlike EF_AD, whose failure just leaves `mnc` unset.
fn read_card_identity(
    card: &mut dyn ApduTransport,
) -> BridgeResult<(String, String, Option<String>)> {
    let aid = usim::discover_usim_aid(card)?;
    usim::select_usim(card, &aid)?;
    let imsi = usim::read_imsi(card)?;
    let mcc = imsi
        .get(..3)
        .ok_or_else(|| BridgeError::Ims(format!("IMSI {imsi:?} shorter than an MCC")))?
        .to_string();
    let mnc = match usim::read_mnc_length(card) {
        Ok(len) => imsi.get(3..3 + len as usize).map(str::to_string),
        Err(e) => {
            tracing::debug!(error = %e, "EF_AD unreadable; MNC left unknown");
            None
        }
    };
    Ok((imsi, mcc, mnc))
}

/// Looks up the operator/country for one MCC/MNC pair. `None` on any
/// failure (network error, no match, unexpected shape) — a lookup failure
/// is informational, not fatal to the listing.
async fn lookup_carrier(client: &reqwest::Client, mcc: &str, mnc: &str) -> Option<String> {
    let resp = client
        .get(CARRIER_LOOKUP_URL)
        .query(&[("mcc", mcc), ("mnc", mnc)])
        .timeout(CARRIER_LOOKUP_TIMEOUT)
        .send()
        .await
        .inspect_err(|e| tracing::debug!(error = %e, mcc, mnc, "carrier lookup request failed"))
        .ok()?;
    let body: LookupResponse = resp
        .json()
        .await
        .inspect_err(
            |e| tracing::debug!(error = %e, mcc, mnc, "carrier lookup response unparseable"),
        )
        .ok()?;
    let entry = body.results.into_iter().next()?;
    Some(format!("{} ({})", entry.operator, entry.country))
}

/// Enumerates every non-vpcd reader and reads whatever card is in it.
/// `client` is passed in (rather than built here) so `run` owns the one
/// tokio runtime + HTTP client for the whole call, and so tests could swap
/// in a client pointed at a mock server if that's ever worth doing.
pub async fn list_cards(client: &reqwest::Client) -> BridgeResult<Vec<CardRow>> {
    let ctx = Context::establish(Scope::User)
        .map_err(|e| BridgeError::Ims(format!("PC/SC context establish failed: {e}")))?;

    let mut readers_buf = [0u8; 2048];
    let readers: Vec<String> = ctx
        .list_readers(&mut readers_buf)
        .map_err(|e| BridgeError::Ims(format!("PC/SC list_readers failed: {e}")))?
        .map(|name| name.to_string_lossy().into_owned())
        .collect();

    let candidates = real_reader_candidates(&readers);
    let mut rows = Vec::with_capacity(candidates.len());
    // Several readers legitimately share one operator's SIMs (a multi-line
    // deployment often does) — cache the lookup per MCC/MNC pair instead of
    // re-querying the API once per reader.
    let mut carrier_cache: HashMap<(String, String), Option<String>> = HashMap::new();

    for reader_name in candidates {
        let reader_cstr = match std::ffi::CString::new(reader_name.as_bytes()) {
            Ok(c) => c,
            Err(e) => {
                rows.push(CardRow {
                    reader: reader_name.clone(),
                    status: format!("reader name has an embedded NUL: {e}"),
                    imsi: None,
                    mcc: None,
                    mnc: None,
                    carrier: None,
                });
                continue;
            }
        };

        let mut card = match ctx.connect(&reader_cstr, ShareMode::Shared, Protocols::ANY) {
            Ok(c) => c,
            Err(pcsc::Error::NoSmartcard) => {
                rows.push(CardRow {
                    reader: reader_name.clone(),
                    status: "no card".to_string(),
                    imsi: None,
                    mcc: None,
                    mnc: None,
                    carrier: None,
                });
                continue;
            }
            Err(e) => {
                rows.push(CardRow {
                    reader: reader_name.clone(),
                    status: format!("connect failed: {e}"),
                    imsi: None,
                    mcc: None,
                    mnc: None,
                    carrier: None,
                });
                continue;
            }
        };

        // Exclusive access for the whole SELECT/READ sequence, the same
        // hazard (and fix) as `PcscTransport::connect`'s candidate probe: a
        // sibling process's APDUs interleaving mid-sequence would corrupt
        // the read.
        let identity = match card.transaction() {
            Ok(mut tx) => {
                let result = read_card_identity(&mut tx);
                let _ = tx.end(Disposition::LeaveCard);
                result
            }
            Err(e) => Err(BridgeError::Ims(format!(
                "PC/SC transaction begin failed: {e}"
            ))),
        };

        match identity {
            Err(e) => rows.push(CardRow {
                reader: reader_name.clone(),
                status: format!("read failed: {e}"),
                imsi: None,
                mcc: None,
                mnc: None,
                carrier: None,
            }),
            Ok((imsi, mcc, mnc)) => {
                let carrier = match &mnc {
                    Some(mnc) => {
                        let key = (mcc.clone(), mnc.clone());
                        if let Some(cached) = carrier_cache.get(&key) {
                            cached.clone()
                        } else {
                            let looked_up = lookup_carrier(client, &mcc, mnc).await;
                            carrier_cache.insert(key, looked_up.clone());
                            looked_up
                        }
                    }
                    None => None,
                };
                rows.push(CardRow {
                    reader: reader_name.clone(),
                    status: if mnc.is_some() {
                        "ok".to_string()
                    } else {
                        "ok (mnc unknown)".to_string()
                    },
                    imsi: Some(imsi),
                    mcc: Some(mcc),
                    mnc,
                    carrier,
                });
            }
        }
    }

    Ok(rows)
}

fn print_table(rows: &[CardRow]) {
    if rows.is_empty() {
        println!(
            "no real PC/SC reader found (pcscd sees only vpcd virtual slots or nothing at all)"
        );
        return;
    }
    println!(
        "{:<40} {:<20} {:<17} {:<5} {:<5} carrier",
        "reader", "status", "imsi", "mcc", "mnc"
    );
    println!("{}", "-".repeat(100));
    for row in rows {
        println!(
            "{:<40} {:<20} {:<17} {:<5} {:<5} {}",
            row.reader,
            row.status,
            row.imsi.as_deref().unwrap_or("-"),
            row.mcc.as_deref().unwrap_or("-"),
            row.mnc.as_deref().unwrap_or("-"),
            row.carrier.as_deref().unwrap_or("-"),
        );
    }
}

pub fn run() -> ExitCode {
    let rt = match crate::runtime::build_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let client = match reqwest::Client::builder()
        .timeout(CARRIER_LOOKUP_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to build HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(list_cards(&client)) {
        Ok(rows) => {
            print_table(&rows);
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
    use crate::error::BridgeResult;
    use std::collections::VecDeque;

    /// Scripts raw APDU responses in order — the same pattern
    /// `vowifi::plmn`'s card-path tests use.
    struct QueuedCard(VecDeque<String>);

    impl ApduTransport for QueuedCard {
        fn transmit_apdu(&mut self, _apdu_hex: &str) -> BridgeResult<String> {
            self.0
                .pop_front()
                .ok_or_else(|| BridgeError::Ims("no more scripted responses".into()))
        }
    }

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
    fn read_card_identity_reads_imsi_and_plmn() {
        let mut responses = usim_selection_responses();
        responses.extend([
            "9000".to_string(),                   // SELECT EF_IMSI
            "0849403408389946049000".to_string(), // READ BINARY EF_IMSI
            "9000".to_string(),                   // SELECT EF_AD
            "000000029000".to_string(),           // READ BINARY EF_AD -> 2-digit MNC
        ]);
        let mut card = QueuedCard(VecDeque::from(responses));
        let (imsi, mcc, mnc) = read_card_identity(&mut card).unwrap();
        assert_eq!(imsi, "404438083996440");
        assert_eq!(mcc, "404");
        assert_eq!(mnc.as_deref(), Some("43"));
    }

    #[test]
    fn read_card_identity_leaves_mnc_unknown_when_ef_ad_unreadable() {
        let mut responses = usim_selection_responses();
        responses.extend([
            "9000".to_string(),
            "0849403408389946049000".to_string(),
            "6A82".to_string(), // SELECT EF_AD: file not found
        ]);
        let mut card = QueuedCard(VecDeque::from(responses));
        let (imsi, mcc, mnc) = read_card_identity(&mut card).unwrap();
        assert_eq!(imsi, "404438083996440");
        assert_eq!(mcc, "404");
        assert_eq!(mnc, None);
    }

    #[test]
    fn read_card_identity_propagates_imsi_failure() {
        let mut responses = usim_selection_responses();
        responses.push("6A82".to_string()); // SELECT EF_IMSI fails
        let mut card = QueuedCard(VecDeque::from(responses));
        assert!(read_card_identity(&mut card).is_err());
    }

    #[test]
    fn print_table_handles_no_readers_without_panicking() {
        print_table(&[]);
    }

    #[test]
    fn print_table_handles_a_mix_of_statuses() {
        print_table(&[
            CardRow {
                reader: "OMNIKEY AG SmartCard Reader 3x21 00 00".to_string(),
                status: "ok".to_string(),
                imsi: Some("404438083996440".to_string()),
                mcc: Some("404".to_string()),
                mnc: Some("43".to_string()),
                carrier: Some("Vodafone India (India)".to_string()),
            },
            CardRow {
                reader: "OMNIKEY AG SmartCard Reader 3x21 01 00".to_string(),
                status: "no card".to_string(),
                imsi: None,
                mcc: None,
                mnc: None,
                carrier: None,
            },
        ]);
    }
}
