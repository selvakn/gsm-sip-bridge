//! PC/SC transport for a real physical smart-card reader
//! (specs/023-omnikey-pcsc-vowifi), the IMS-AKA registration counterpart to
//! `AtCommander`'s `AT+CSIM` passthrough for a modem-backed line. Both
//! implement `modules::usim::ApduTransport`, so `modules::usim`'s
//! SELECT/READ RECORD/AUTHENTICATE logic runs unchanged over either — a
//! `pcsc_reader` line's only difference is which transport carries the APDUs.
//!
//! strongSwan's `eap-sim-pcsc` plugin matches a reader to a line by IMSI
//! substring against the NAI (`Dockerfile`'s credit note) so a mixed or
//! multi-reader deployment always authenticates the right SIM for the right
//! line. Our own IMS-AKA registration needs the same disambiguation: with
//! two or more `pcsc_reader` lines (each configured with its own
//! `imsi_override`), picking merely "the first non-vpcd reader" would let
//! every line's IMS session connect to the same physical card, causing the
//! others to authenticate as the wrong subscriber. `connect` therefore
//! probes every non-vpcd reader's card's own EF_IMSI and returns the one
//! whose IMSI matches the line it was asked for.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::usim::{self, ApduTransport};
use pcsc::{Card, Context, Disposition, Protocols, Scope, ShareMode, Transaction, MAX_BUFFER_SIZE};

/// Reader names pcscd assigns its virtual vpcd slots — never a real reader
/// we should connect to for IMS-AKA.
const VPCD_READER_MARKER: &str = "Virtual PCD";

pub struct PcscTransport {
    card: Card,
}

/// Every reader name that isn't a vpcd virtual slot — the candidates worth
/// probing for a matching card.
fn real_reader_candidates(readers: &[String]) -> Vec<&String> {
    readers
        .iter()
        .filter(|n| !n.contains(VPCD_READER_MARKER))
        .collect()
}

impl PcscTransport {
    /// Connects to whichever non-vpcd PC/SC reader holds a card whose own
    /// EF_IMSI matches `target_imsi` — this line's configured IMSI
    /// (specs/023-omnikey-pcsc-vowifi requires `imsi_override` on every
    /// `pcsc_reader` line, so this is always known up front). Tries each
    /// candidate reader in turn; a reader with no card, an unreadable card,
    /// or a card for a different line is skipped rather than treated as
    /// fatal, since `pcscd` legitimately reports many candidates.
    pub fn connect(target_imsi: &str) -> BridgeResult<Self> {
        let ctx = Context::establish(Scope::User)
            .map_err(|e| BridgeError::Ims(format!("PC/SC context establish failed: {e}")))?;

        let mut readers_buf = [0u8; 2048];
        let readers: Vec<String> = ctx
            .list_readers(&mut readers_buf)
            .map_err(|e| BridgeError::Ims(format!("PC/SC list_readers failed: {e}")))?
            .map(|name| name.to_string_lossy().into_owned())
            .collect();

        let candidates = real_reader_candidates(&readers);
        if candidates.is_empty() {
            return Err(BridgeError::Ims(format!(
                "no real PC/SC reader found (pcscd sees only vpcd virtual slots \
                 or nothing at all); readers seen: {readers:?}"
            )));
        }

        let mut imsis_seen = Vec::new();
        for reader_name in &candidates {
            let reader_cstr = std::ffi::CString::new(reader_name.as_bytes()).map_err(|e| {
                BridgeError::Ims(format!(
                    "reader name {reader_name:?} has an embedded NUL: {e}"
                ))
            })?;
            let mut card = match ctx.connect(&reader_cstr, ShareMode::Shared, Protocols::ANY) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(reader = %reader_name, error = %e, "PC/SC connect failed, trying next reader");
                    continue;
                }
            };
            // `ShareMode::Shared` lets more than one client connect to the
            // same physical reader at once (e.g. a sibling pcsc_reader
            // line's own `connect()` probing the same candidate list
            // concurrently at startup) — without a PC/SC transaction, their
            // SELECT/READ APDUs can interleave on the wire and corrupt each
            // other's reads, causing a probe to misidentify or skip the
            // reader that's actually its match (caught in review). Each
            // candidate's whole discover-AID/select-ADF/read-IMSI sequence
            // therefore runs inside one transaction, giving it exclusive
            // access to the card for its duration.
            let probe = match card.transaction() {
                Ok(mut tx) => {
                    let result = Self::read_transport_imsi(&mut tx);
                    let _ = tx.end(Disposition::LeaveCard);
                    result
                }
                Err(e) => Err(BridgeError::Ims(format!(
                    "PC/SC transaction begin failed on reader {reader_name:?}: {e}"
                ))),
            };
            match probe {
                Ok(imsi) if imsi == target_imsi => {
                    tracing::info!(reader = %reader_name, "connected to PC/SC reader");
                    return Ok(Self { card });
                }
                Ok(other_imsi) => {
                    tracing::debug!(
                        reader = %reader_name,
                        imsi = %other_imsi,
                        "reader's card IMSI doesn't match this line; trying next reader"
                    );
                    imsis_seen.push(other_imsi);
                }
                Err(e) => {
                    tracing::warn!(reader = %reader_name, error = %e, "failed to read this reader's card IMSI, trying next reader");
                }
            }
        }

        Err(BridgeError::Ims(format!(
            "no PC/SC reader has a card matching IMSI {target_imsi} \
             (readers checked: {candidates:?}, IMSIs seen: {imsis_seen:?})"
        )))
    }

    fn read_transport_imsi(t: &mut dyn ApduTransport) -> BridgeResult<String> {
        let aid = usim::discover_usim_aid(t)?;
        usim::select_usim(t, &aid)?;
        usim::read_imsi(t)
    }
}

impl ApduTransport for PcscTransport {
    fn transmit_apdu(&mut self, apdu_hex: &str) -> BridgeResult<String> {
        let apdu = crate::modules::usim::hex_decode(apdu_hex)?;
        let mut recv_buf = [0u8; MAX_BUFFER_SIZE];
        let rapdu = self
            .card
            .transmit(&apdu, &mut recv_buf)
            .map_err(|e| BridgeError::Ims(format!("PC/SC transmit failed: {e}")))?;
        Ok(crate::modules::usim::hex_encode(rapdu))
    }
}

/// Lets a probe run through the same `usim::*` SELECT/READ logic while a
/// transaction is held, without a second copy of `ApduTransport`'s body —
/// `Transaction` derefs to `Card`, whose `transmit` is the same primitive
/// `PcscTransport` already wraps.
impl ApduTransport for Transaction<'_> {
    fn transmit_apdu(&mut self, apdu_hex: &str) -> BridgeResult<String> {
        let apdu = crate::modules::usim::hex_decode(apdu_hex)?;
        let mut recv_buf = [0u8; MAX_BUFFER_SIZE];
        let rapdu = self
            .transmit(&apdu, &mut recv_buf)
            .map_err(|e| BridgeError::Ims(format!("PC/SC transmit failed: {e}")))?;
        Ok(crate::modules::usim::hex_encode(rapdu))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_only_real_reader_among_vpcd_slots() {
        let readers = vec![
            "Virtual PCD 00 00".to_string(),
            "Virtual PCD 01 00".to_string(),
            "OMNIKEY AG SmartCard Reader 3x21 00 00".to_string(),
            "Virtual PCD 02 00".to_string(),
        ];
        assert_eq!(
            real_reader_candidates(&readers),
            vec![&"OMNIKEY AG SmartCard Reader 3x21 00 00".to_string()]
        );
    }

    #[test]
    fn finds_every_real_reader_when_more_than_one_is_attached() {
        // The multi-reader case connect() must disambiguate by IMSI across
        // (caught by review: a fixed "pick the first" here would silently
        // authenticate every pcsc_reader line as the same subscriber).
        let readers = vec![
            "Virtual PCD 00 00".to_string(),
            "OMNIKEY AG SmartCard Reader 3x21 00 00".to_string(),
            "OMNIKEY AG SmartCard Reader 3x21 01 00".to_string(),
        ];
        assert_eq!(
            real_reader_candidates(&readers),
            vec![
                &"OMNIKEY AG SmartCard Reader 3x21 00 00".to_string(),
                &"OMNIKEY AG SmartCard Reader 3x21 01 00".to_string(),
            ]
        );
    }

    #[test]
    fn empty_when_only_vpcd_slots_are_present() {
        let readers = vec![
            "Virtual PCD 00 00".to_string(),
            "Virtual PCD 01 00".to_string(),
        ];
        assert!(real_reader_candidates(&readers).is_empty());
    }

    #[test]
    fn empty_when_no_readers_at_all() {
        assert!(real_reader_candidates(&[]).is_empty());
    }
}
