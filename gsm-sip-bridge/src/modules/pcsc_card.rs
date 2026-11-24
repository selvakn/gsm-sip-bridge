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
use std::ffi::CStr;
use std::ops::{Deref, DerefMut};

/// Reader names pcscd assigns its virtual vpcd slots — never a real reader
/// we should connect to for IMS-AKA.
const VPCD_READER_MARKER: &str = "Virtual PCD";

/// A shared PC/SC connection that is always released *leaving* the card.
///
/// The `pcsc` crate's own `Card::drop` hard-codes `SCARD_RESET_CARD`
/// (pcsc 2.9.0 `lib.rs:1668`). A reset invalidates every other program's
/// session on the same SIM — charon's `eap-sim-pcsc`, a sibling line, or
/// this line's own re-authentication — whose next command then fails with
/// "The smart card has been reset" (#232). Every PC/SC connection the bridge
/// opens goes through this type, never a bare `Card`, so every exit path
/// (normal scope end, `?`, an early `continue`, a panic's unwind) releases
/// with `Disposition::LeaveCard`.
///
/// If `SCardDisconnect` itself fails, the returned `Card` is
/// `std::mem::forget`ten instead of dropped, so the resetting `Drop` never
/// runs. That leaks one `Context` reference until the process exits and
/// nothing else: the handle is already unusable, and pcscd's own cleanup of
/// an abandoned handle leaves the card.
pub(crate) struct SharedCard(Option<Card>);

impl SharedCard {
    /// Connects to `reader` in shared mode with any protocol. The raw
    /// `pcsc::Error` is kept so callers can still match on it
    /// (`pcsc_list` treats `NoSmartcard` as "no card").
    pub(crate) fn connect(ctx: &Context, reader: &CStr) -> Result<SharedCard, pcsc::Error> {
        ctx.connect(reader, ShareMode::Shared, Protocols::ANY)
            .map(|card| SharedCard(Some(card)))
    }
}

impl Deref for SharedCard {
    type Target = Card;

    fn deref(&self) -> &Card {
        self.0.as_ref().expect("SharedCard used after release")
    }
}

impl DerefMut for SharedCard {
    fn deref_mut(&mut self) -> &mut Card {
        self.0.as_mut().expect("SharedCard used after release")
    }
}

impl Drop for SharedCard {
    fn drop(&mut self) {
        if let Some(card) = self.0.take() {
            if let Err((card, _)) = card.disconnect(Disposition::LeaveCard) {
                std::mem::forget(card);
            }
        }
    }
}

pub struct PcscTransport {
    card: SharedCard,
    /// The USIM application's AID, found by the probe that identified this
    /// card in `connect`. `authenticate_usim` selects it inside each AKA
    /// transaction. It never changes: a swapped card fails the next command
    /// whatever we cached.
    aid: Vec<u8>,
}

/// Every reader name that isn't a vpcd virtual slot — the candidates worth
/// probing for a matching card. `pub(crate)` so `modules::pcsc_list` (which
/// enumerates every real reader rather than hunting for one matching IMSI)
/// can reuse the same vpcd filter instead of re-implementing it.
pub(crate) fn real_reader_candidates(readers: &[String]) -> Vec<&String> {
    readers
        .iter()
        .filter(|n| !n.contains(VPCD_READER_MARKER))
        .collect()
}

impl PcscTransport {
    /// Connects to whichever non-vpcd PC/SC reader holds a card whose own
    /// EF_IMSI matches `target_imsi` — this line's configured IMSI
    /// (`imsi_override` is mandatory on every `pcsc_reader` line precisely so
    /// this binding is known before any card session exists, which is what
    /// makes the probe below possible at all). Tries each
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
        // Every candidate this loop rejects (no match, unreadable card, failed
        // transaction) is released leaving its card when its `SharedCard`
        // drops at the end of the iteration: another line may be mid-AKA on
        // it (#232).
        for reader_name in &candidates {
            let reader_cstr = std::ffi::CString::new(reader_name.as_bytes()).map_err(|e| {
                BridgeError::Ims(format!(
                    "reader name {reader_name:?} has an embedded NUL: {e}"
                ))
            })?;
            let mut card = match SharedCard::connect(&ctx, &reader_cstr) {
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
                    let result = Self::read_transport_identity(&mut tx);
                    let _ = tx.end(Disposition::LeaveCard);
                    result
                }
                Err(e) => Err(BridgeError::Ims(format!(
                    "PC/SC transaction begin failed on reader {reader_name:?}: {e}"
                ))),
            };
            match probe {
                Ok((aid, imsi)) if imsi == target_imsi => {
                    tracing::info!(reader = %reader_name, "connected to PC/SC reader");
                    return Ok(Self { card, aid });
                }
                Ok((_, other_imsi)) => {
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

    /// The card's USIM AID and its IMSI, read in one pass: the AID is what
    /// `authenticate_usim` selects later, the IMSI is what `connect` matches.
    fn read_transport_identity(t: &mut dyn ApduTransport) -> BridgeResult<(Vec<u8>, String)> {
        let aid = usim::discover_usim_aid(t)?;
        usim::select_usim(t, &aid)?;
        let imsi = usim::read_imsi(t)?;
        Ok((aid, imsi))
    }

    /// Runs `f` with exclusive access to the card, for a multi-step sequence
    /// whose correctness depends on nothing else re-selecting a file in
    /// between.
    ///
    /// `ShareMode::Shared` means charon's `eap-sim-pcsc` (or a sibling line)
    /// can be mid-EAP on this same card; an interleaved SELECT landing
    /// between our own SELECT and READ BINARY would have us read the wrong
    /// file and believe it — the same hazard the candidate probe in
    /// `connect` already holds a transaction for. AUTHENTICATE has the same
    /// need: it is addressed to the selected USIM ADF, and its GET RESPONSE
    /// must follow on the same channel, which is why `authenticate_usim`
    /// runs SELECT and AUTHENTICATE inside one transaction (#232).
    pub fn with_transaction<T>(
        &mut self,
        f: impl FnOnce(&mut dyn ApduTransport) -> BridgeResult<T>,
    ) -> BridgeResult<T> {
        let mut tx = self
            .card
            .transaction()
            .map_err(|e| BridgeError::Ims(format!("PC/SC transaction begin failed: {e}")))?;
        let result = f(&mut tx);
        let _ = tx.end(Disposition::LeaveCard);
        result
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

    /// Selects the USIM and authenticates inside one PC/SC transaction, so
    /// no other program's SELECT (or reset) can land between our SELECT,
    /// AUTHENTICATE and GET RESPONSE. Other programs wait for the
    /// transaction, which lasts only for these four commands. Each call is
    /// its own transaction, so a resynchronisation gets a fresh one.
    fn authenticate_usim(
        &mut self,
        rand: &[u8; 16],
        autn: &[u8; 16],
    ) -> BridgeResult<usim::AkaResult> {
        let aid = self.aid.clone();
        self.with_transaction(|t| usim::select_and_authenticate(t, &aid, rand, autn))
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

/// Hardware-gated tests against the real pcscd and a real reader with a SIM
/// (#232). No mocks: the point is the release disposition pcscd sees. They
/// share one physical card, so run them with `--test-threads=1`:
/// `cargo test -p gsm-sip-bridge pcsc_card::hardware -- --ignored --test-threads=1`.
#[cfg(test)]
mod hardware {
    use super::*;
    use std::ffi::CString;

    /// SELECT MF asking for the FCP: a harmless read-only command whose
    /// answer is `9000` (T=1) or `61xx` (T=0).
    const SELECT_MF: [u8; 7] = [0x00, 0xA4, 0x00, 0x04, 0x02, 0x3F, 0x00];

    fn context() -> Context {
        Context::establish(Scope::User).expect("pcscd is not running")
    }

    fn first_real_reader(ctx: &Context) -> CString {
        let mut buf = [0u8; 2048];
        let readers: Vec<String> = ctx
            .list_readers(&mut buf)
            .expect("PC/SC list_readers failed")
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        let reader = real_reader_candidates(&readers)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no real PC/SC reader attached; readers seen: {readers:?}"))
            .clone();
        CString::new(reader).expect("reader name has an embedded NUL")
    }

    /// Another program's session on the card: a bare `Card` (deliberately not
    /// `SharedCard`, so it does not depend on the code under test), connected
    /// shared, with one command already sent inside a transaction.
    fn holder(ctx: &Context, reader: &CStr) -> Card {
        let mut card = ctx
            .connect(reader, ShareMode::Shared, Protocols::ANY)
            .expect("holder connect failed: is a SIM in the reader?");
        let tx = card.transaction().expect("holder transaction failed");
        let mut buf = [0u8; MAX_BUFFER_SIZE];
        tx.transmit(&SELECT_MF, &mut buf)
            .expect("holder's first transmit failed");
        tx.end(Disposition::LeaveCard)
            .map_err(|(_, e)| e)
            .expect("holder end transaction failed");
        card
    }

    /// The holder's next command, as status words or the PC/SC error.
    fn holder_transmit(holder: &Card) -> Result<String, pcsc::Error> {
        let mut buf = [0u8; MAX_BUFFER_SIZE];
        holder
            .transmit(&SELECT_MF, &mut buf)
            .map(crate::modules::usim::hex_encode)
    }

    fn assert_holder_still_works(holder: &Card) {
        let resp = holder_transmit(holder).expect("the holder's session was disturbed");
        let sw = &resp[resp.len().saturating_sub(4)..];
        assert!(
            sw == "9000" || sw.starts_with("61"),
            "unexpected SW from the holder's SELECT MF: {resp}"
        );
    }

    /// Releases the holder leaving the card, so the test itself does not
    /// reset the SIM for whatever runs next.
    fn release(holder: Card) {
        if let Err((card, _)) = holder.disconnect(Disposition::LeaveCard) {
            std::mem::forget(card);
        }
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn bare_card_drop_resets_other_handles() {
        // The control: proves this rig can see the bug the other tests guard.
        let holder_ctx = context();
        let reader = first_real_reader(&holder_ctx);
        let holder = holder(&holder_ctx, &reader);

        let other_ctx = context();
        let bare = other_ctx
            .connect(&reader, ShareMode::Shared, Protocols::ANY)
            .expect("second connect failed");
        drop(bare);

        assert_eq!(holder_transmit(&holder), Err(pcsc::Error::ResetCard));
        release(holder);
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn shared_card_drop_leaves_other_handles_working() {
        let holder_ctx = context();
        let reader = first_real_reader(&holder_ctx);
        let holder = holder(&holder_ctx, &reader);

        let other_ctx = context();
        let shared = SharedCard::connect(&other_ctx, &reader).expect("second connect failed");
        drop(shared);

        assert_holder_still_works(&holder);
        release(holder);
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn shared_card_released_by_panic_leaves_the_card() {
        let holder_ctx = context();
        let reader = first_real_reader(&holder_ctx);
        let holder = holder(&holder_ctx, &reader);

        let other_ctx = context();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _shared = SharedCard::connect(&other_ctx, &reader).expect("second connect failed");
            panic!("deliberate panic while the SharedCard is held");
        }));
        assert!(unwound.is_err());

        assert_holder_still_works(&holder);
        release(holder);
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn connect_for_another_imsi_leaves_the_card() {
        // A sibling line's search: it probes this card, finds the wrong IMSI
        // and moves on. The rejected candidate must be released leaving it.
        let holder_ctx = context();
        let reader = first_real_reader(&holder_ctx);
        let holder = holder(&holder_ctx, &reader);

        let Err(e) = PcscTransport::connect("001010000000001") else {
            panic!("a card with the synthetic test IMSI is in the reader");
        };
        assert!(
            e.to_string()
                .contains("no PC/SC reader has a card matching"),
            "unexpected error: {e}"
        );

        assert_holder_still_works(&holder);
        release(holder);
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn authenticate_usim_runs_in_one_window_and_leaves_the_card() {
        let holder_ctx = context();
        let reader = first_real_reader(&holder_ctx);

        // The card's own IMSI, read at run time: never hard-coded.
        let imsi = {
            let mut card = SharedCard::connect(&holder_ctx, &reader).expect("connect failed");
            let mut tx = card.transaction().expect("transaction failed");
            let (_, imsi) =
                PcscTransport::read_transport_identity(&mut tx).expect("reading the IMSI failed");
            let _ = tx.end(Disposition::LeaveCard);
            imsi
        };

        // Held across the whole AKA: a reset at the window's end or at the
        // transport's release would break its next command.
        let holder = holder(&holder_ctx, &reader);

        let mut transport = PcscTransport::connect(&imsi).expect("connect by IMSI failed");
        // A bogus AUTN: the card answers with a MAC failure, which is harmless
        // (no SQN change). What matters is that the error comes from the card
        // inside the window, not from starting it or from a transmit.
        let Err(e) = transport.authenticate_usim(&[0x11; 16], &[0x22; 16]) else {
            panic!("the card accepted a bogus AUTN");
        };
        let msg = e.to_string();
        assert!(
            msg.contains("AUTHENTICATE failed"),
            "unexpected error: {msg}"
        );
        drop(transport);

        assert_holder_still_works(&holder);
        release(holder);
    }
}
