//! PC/SC transport for a real physical smart-card reader
//! (specs/023-omnikey-pcsc-vowifi), the IMS-AKA registration counterpart to
//! `AtCommander`'s `AT+CSIM` passthrough for a modem-backed line. Both
//! implement `modules::usim::ApduTransport`, so `modules::usim`'s
//! SELECT/READ RECORD/AUTHENTICATE logic runs unchanged over either — a
//! `pcsc_reader` line's only difference is which transport carries the APDUs.
//!
//! strongSwan's `eap-sim-pcsc` plugin matches a reader to a line by IMSI
//! substring against the NAI (`Dockerfile`'s credit note); our own IMS-AKA
//! registration has no such multi-reader arbitration; it connects to the
//! first reader that isn't one of pcscd's `vpcd` virtual slots (named
//! "Virtual PCD ..." — see `supervise::render`'s vpcd `reader.conf.d`
//! template), which is correct as long as at most one *real* reader is
//! attached (specs/023-omnikey-pcsc-vowifi's scope: a single OmniKey AG
//! 3x21). A second real reader would need the same IMSI-substring
//! disambiguation `eap-sim-pcsc` already does, which is out of scope here.

use crate::error::{BridgeError, BridgeResult};
use crate::modules::usim::ApduTransport;
use pcsc::{Card, Context, Protocols, Scope, ShareMode, MAX_BUFFER_SIZE};

/// Reader names pcscd assigns its virtual vpcd slots — never a real reader
/// we should connect to for IMS-AKA.
const VPCD_READER_MARKER: &str = "Virtual PCD";

pub struct PcscTransport {
    card: Card,
}

/// Picks the first reader name that isn't a vpcd virtual slot — the
/// arbitration this module needs (see the module doc comment on scope).
fn select_real_reader(readers: &[String]) -> Option<&String> {
    readers.iter().find(|n| !n.contains(VPCD_READER_MARKER))
}

impl PcscTransport {
    /// Connects to the first non-vpcd PC/SC reader pcscd exposes.
    pub fn connect() -> BridgeResult<Self> {
        let ctx = Context::establish(Scope::User)
            .map_err(|e| BridgeError::Ims(format!("PC/SC context establish failed: {e}")))?;

        let mut readers_buf = [0u8; 2048];
        let readers: Vec<_> = ctx
            .list_readers(&mut readers_buf)
            .map_err(|e| BridgeError::Ims(format!("PC/SC list_readers failed: {e}")))?
            .map(|name| name.to_string_lossy().into_owned())
            .collect();

        let Some(reader_name) = select_real_reader(&readers) else {
            return Err(BridgeError::Ims(format!(
                "no real PC/SC reader found (pcscd sees only vpcd virtual slots \
                 or nothing at all); readers seen: {readers:?}"
            )));
        };
        let reader_cstr = std::ffi::CString::new(reader_name.as_bytes()).map_err(|e| {
            BridgeError::Ims(format!(
                "reader name {reader_name:?} has an embedded NUL: {e}"
            ))
        })?;

        let card = ctx
            .connect(&reader_cstr, ShareMode::Shared, Protocols::ANY)
            .map_err(|e| {
                BridgeError::Ims(format!(
                    "PC/SC connect to reader {reader_name:?} failed: {e}"
                ))
            })?;

        tracing::info!(reader = %reader_name, "connected to PC/SC reader");
        Ok(Self { card })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_only_real_reader_among_vpcd_slots() {
        let readers = vec![
            "Virtual PCD 00 00".to_string(),
            "Virtual PCD 01 00".to_string(),
            "OMNIKEY AG SmartCard Reader 3x21 00 00".to_string(),
            "Virtual PCD 02 00".to_string(),
        ];
        assert_eq!(
            select_real_reader(&readers),
            Some(&"OMNIKEY AG SmartCard Reader 3x21 00 00".to_string())
        );
    }

    #[test]
    fn none_when_only_vpcd_slots_are_present() {
        let readers = vec![
            "Virtual PCD 00 00".to_string(),
            "Virtual PCD 01 00".to_string(),
        ];
        assert_eq!(select_real_reader(&readers), None);
    }

    #[test]
    fn none_when_no_readers_at_all() {
        assert_eq!(select_real_reader(&[]), None);
    }
}
