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

/// The PC/SC failures that mean the card cannot be reached right now, as
/// opposed to a fault in how we called PC/SC (#233):
///
/// - the card was reset by another program, pulled, is unpowered or does
///   not answer (`ResetCard`, `RemovedCard`, `NoSmartcard`, `UnpoweredCard`,
///   `UnresponsiveCard`);
/// - its reader went away or the link to it broke (`ReaderUnavailable`,
///   `UnknownReader`, `CommError`, `CommDataLost`, `NoReadersAvailable`);
/// - pcscd is not running or is shutting down (`NoService`,
///   `ServiceStopped`, `Shutdown`), which also invalidates our handle
///   (`InvalidHandle`);
/// - another program holds the card exclusively (`SharingViolation`).
///
/// Every other error (a timeout we asked for, a bad parameter, a protocol
/// mismatch, an internal error) is not about reachability and keeps today's
/// untyped handling.
fn means_unreachable(e: pcsc::Error) -> bool {
    use pcsc::Error as E;
    matches!(
        e,
        E::ResetCard
            | E::RemovedCard
            | E::NoSmartcard
            | E::UnpoweredCard
            | E::UnresponsiveCard
            | E::ReaderUnavailable
            | E::UnknownReader
            | E::CommError
            | E::CommDataLost
            | E::NoReadersAvailable
            | E::NoService
            | E::ServiceStopped
            | E::Shutdown
            | E::InvalidHandle
            | E::SharingViolation
    )
}

/// The `BridgeError` for a failed PC/SC call, `context: e` either way: a
/// [`BridgeError::CardUnreachable`] when the failure means the card cannot be
/// reached ([`means_unreachable`]), otherwise the untyped `Ims` error the
/// transport always returned. Every PC/SC failure the transport returns goes
/// through here, so the type is decided in one place (#233).
pub(crate) fn pcsc_failure(context: &str, e: pcsc::Error) -> BridgeError {
    let text = format!("{context}: {e}");
    if means_unreachable(e) {
        BridgeError::CardUnreachable(text)
    } else {
        BridgeError::Ims(text)
    }
}

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
            .map_err(|e| pcsc_failure("PC/SC context establish failed", e))?;

        let mut readers_buf = [0u8; 2048];
        let readers: Vec<String> = ctx
            .list_readers(&mut readers_buf)
            .map_err(|e| pcsc_failure("PC/SC list_readers failed", e))?
            .map(|name| name.to_string_lossy().into_owned())
            .collect();

        let candidates = real_reader_candidates(&readers);
        // No reader at all is an unreachable card too: the reader may be
        // re-enumerating, or unplugged until the operator puts it back.
        if candidates.is_empty() {
            return Err(BridgeError::CardUnreachable(format!(
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

        // Our card is not in any reader right now (pulled, or a reader that
        // failed to answer the probe): unreachable, not a fault.
        Err(BridgeError::CardUnreachable(format!(
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
            .map_err(|e| pcsc_failure("PC/SC transaction begin failed", e))?;
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
            .map_err(|e| pcsc_failure("PC/SC transmit failed", e))?;
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
            .map_err(|e| pcsc_failure("PC/SC transmit failed", e))?;
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

    /// The PC/SC errors that mean the card cannot be reached (#233).
    const UNREACHABLE: [pcsc::Error; 15] = [
        pcsc::Error::ResetCard,
        pcsc::Error::RemovedCard,
        pcsc::Error::NoSmartcard,
        pcsc::Error::UnpoweredCard,
        pcsc::Error::UnresponsiveCard,
        pcsc::Error::ReaderUnavailable,
        pcsc::Error::UnknownReader,
        pcsc::Error::CommError,
        pcsc::Error::CommDataLost,
        pcsc::Error::NoReadersAvailable,
        pcsc::Error::NoService,
        pcsc::Error::ServiceStopped,
        pcsc::Error::Shutdown,
        pcsc::Error::InvalidHandle,
        pcsc::Error::SharingViolation,
    ];

    #[test]
    fn unreachable_pcsc_errors_are_typed() {
        for e in UNREACHABLE {
            let err = pcsc_failure("PC/SC transmit failed", e);
            assert!(err.is_card_unreachable(), "{e:?} -> {err:?}");
            assert!(
                err.to_string()
                    .ends_with(&format!("PC/SC transmit failed: {e}")),
                "{err}"
            );
        }
    }

    #[test]
    fn other_pcsc_errors_stay_untyped() {
        for e in [
            pcsc::Error::Timeout,
            pcsc::Error::InvalidParameter,
            pcsc::Error::ProtoMismatch,
            pcsc::Error::NotTransacted,
            pcsc::Error::InternalError,
        ] {
            match pcsc_failure("PC/SC transmit failed", e) {
                BridgeError::Ims(text) => assert_eq!(text, format!("PC/SC transmit failed: {e}")),
                other => panic!("{e:?} became {other:?}"),
            }
        }
    }

    /// The registration paths classify failures by their text. The typed
    /// error must get the same verdict from each classifier as the untyped
    /// one it replaces, or a reader outage would change how a failure is
    /// reported or which P-CSCF is blamed.
    #[test]
    fn typed_text_keeps_every_classifier_verdict() {
        use crate::ims::{classify_register_failure, session::map_registration_error};
        let mut texts: Vec<String> = UNREACHABLE
            .iter()
            .map(|e| format!("PC/SC transmit failed: {e}"))
            .collect();
        texts.push(
            "no PC/SC reader has a card matching IMSI 001010000000001 \
             (readers checked: [\"OMNIKEY\"], IMSIs seen: [])"
                .to_string(),
        );
        texts.push("no real PC/SC reader found; readers seen: []".to_string());
        for text in texts {
            let before = BridgeError::Ims(text.clone());
            let after = BridgeError::CardUnreachable(text);
            assert_eq!(
                map_registration_error(&after),
                map_registration_error(&before),
                "{after}"
            );
            assert_eq!(
                classify_register_failure(&after),
                classify_register_failure(&before),
                "{after}"
            );
        }
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

    /// The card's own IMSI, read at run time: never hard-coded.
    fn card_imsi(ctx: &Context, reader: &CStr) -> String {
        let mut card = SharedCard::connect(ctx, reader).expect("connect failed");
        let mut tx = card.transaction().expect("transaction failed");
        let (_, imsi) =
            PcscTransport::read_transport_identity(&mut tx).expect("reading the IMSI failed");
        let _ = tx.end(Disposition::LeaveCard);
        imsi
    }

    /// Another program sharing the card that ignores transactions: on its
    /// own connection it sends `SELECT MF` back to back, with no transaction,
    /// until stopped. One of these landing inside a multi-command sequence
    /// of ours moves the card's current file under it: an AUTHENTICATE then
    /// meets the MF instead of the USIM ADF, and a READ BINARY no longer
    /// reads EF_IMSI. A sequence held in one PC/SC transaction makes the
    /// competitor wait instead.
    struct Competitor {
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<usize>>,
    }

    impl Competitor {
        fn start(reader: &CStr) -> Self {
            use std::sync::atomic::{AtomicBool, Ordering};
            let reader = reader.to_owned();
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let thread = {
                let stop = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    let ctx = context();
                    let card = SharedCard::connect(&ctx, &reader).expect("competitor connect");
                    let select_mf = [0x00, 0xA4, 0x00, 0x0C, 0x02, 0x3F, 0x00];
                    let mut buf = [0u8; MAX_BUFFER_SIZE];
                    let mut sent = 0;
                    ready_tx.send(()).unwrap();
                    while !stop.load(Ordering::SeqCst) {
                        if card.transmit(&select_mf, &mut buf).is_ok() {
                            sent += 1;
                        }
                    }
                    sent
                })
            };
            ready_rx
                .recv()
                .expect("competitor thread died before connecting");
            Self {
                stop,
                thread: Some(thread),
            }
        }

        /// Stops the competitor and returns how many commands it got through.
        fn stop(mut self) -> usize {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            self.thread
                .take()
                .unwrap()
                .join()
                .expect("competitor panicked")
        }
    }

    impl Drop for Competitor {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn a_reset_by_another_program_is_card_unreachable() {
        // #233: another program resetting the card is the commonest way a
        // card-reader line loses its SIM for a moment. Our next command must
        // say "unreachable", so the line retries instead of giving up.
        let ctx = context();
        let reader = first_real_reader(&ctx);
        let imsi = card_imsi(&ctx, &reader);
        let mut transport = PcscTransport::connect(&imsi).expect("connect by IMSI failed");

        let other = context();
        let resetter = other
            .connect(&reader, ShareMode::Shared, Protocols::ANY)
            .expect("second connect failed");
        // A bare `Card` drop releases with `SCARD_RESET_CARD`.
        drop(resetter);

        let err = transport
            .transmit_apdu("00A4000C023F00")
            .expect_err("a command after another program's reset succeeded");
        assert!(err.is_card_unreachable(), "{err:?}");
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn no_reader_with_this_imsi_is_card_unreachable() {
        let err = match PcscTransport::connect("001010000000001") {
            Ok(_) => panic!("a reader holds the synthetic test IMSI"),
            Err(e) => e,
        };
        assert!(err.is_card_unreachable(), "{err:?}");
    }

    /// Rounds of each sequence run against the competitor.
    const COMPETED_ROUNDS: usize = 20;

    /// Runs `round` [`COMPETED_ROUNDS`] times with a pause before each, so
    /// the competitor is mid-stream when each of our sequences starts. Back to
    /// back, our own transactions would starve it of turns and the test would
    /// prove nothing.
    fn competed_rounds<T>(mut round: impl FnMut() -> T) -> Vec<T> {
        (0..COMPETED_ROUNDS)
            .map(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                round()
            })
            .collect()
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn authenticate_usim_window_excludes_a_competing_session() {
        let ctx = context();
        let reader = first_real_reader(&ctx);
        let imsi = card_imsi(&ctx, &reader);
        let mut transport = PcscTransport::connect(&imsi).expect("connect by IMSI failed");

        // Uncontended, a bogus AUTN meets the USIM ADF and gets the card's
        // MAC-failure answer (harmless, no SQN change).
        let aka = |t: &mut PcscTransport| match t.authenticate_usim(&[0x11; 16], &[0x22; 16]) {
            Ok(_) => panic!("the card accepted a bogus AUTN"),
            Err(e) => e.to_string(),
        };
        let alone = aka(&mut transport);
        assert!(
            alone.contains("AUTHENTICATE failed"),
            "unexpected error: {alone}"
        );

        let competitor = Competitor::start(&reader);
        let contended = competed_rounds(|| aka(&mut transport));
        let competing_commands = competitor.stop();

        assert!(
            competing_commands >= COMPETED_ROUNDS,
            "the competitor barely ran ({competing_commands} commands), so this proved nothing"
        );
        // Every round must have met the USIM ADF, exactly as it did alone: a
        // competing SELECT MF between our SELECT AID and AUTHENTICATE would
        // change the card's answer.
        let interleaved: Vec<&String> = contended.iter().filter(|m| **m != alone).collect();
        assert!(
            interleaved.is_empty(),
            "{} of {COMPETED_ROUNDS} AKAs were interleaved (alone: {alone}); first: {}",
            interleaved.len(),
            interleaved[0]
        );
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn connect_probe_excludes_a_competing_session() {
        // The probe that identifies the card also finds the AID the AKA window
        // selects later, so an interleaved read would misidentify the card or
        // cache a wrong AID.
        let ctx = context();
        let reader = first_real_reader(&ctx);
        let imsi = card_imsi(&ctx, &reader);
        let aid = PcscTransport::connect(&imsi)
            .expect("connect by IMSI failed")
            .aid
            .clone();

        let competitor = Competitor::start(&reader);
        let failures: Vec<String> = competed_rounds(|| match PcscTransport::connect(&imsi) {
            Ok(t) if t.aid == aid => None,
            Ok(t) => Some(format!("wrong AID {}", usim::hex_encode(&t.aid))),
            Err(e) => Some(e.to_string()),
        })
        .into_iter()
        .flatten()
        .collect();
        let competing_commands = competitor.stop();

        assert!(
            competing_commands >= COMPETED_ROUNDS,
            "the competitor barely ran ({competing_commands} commands), so this proved nothing"
        );
        assert!(
            failures.is_empty(),
            "{} of {COMPETED_ROUNDS} probes were interleaved; first: {}",
            failures.len(),
            failures[0]
        );
    }

    #[test]
    #[ignore = "needs a PC/SC reader with a SIM; run with --ignored on the reader host"]
    fn pcsc_list_read_excludes_a_competing_session() {
        let ctx = context();
        let reader = first_real_reader(&ctx);
        let reader_name = reader.to_string_lossy().into_owned();
        let imsi = card_imsi(&ctx, &reader);

        // The carrier lookup goes to a closed local port and fails at once:
        // this test is about the card read, not the network.
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all("http://127.0.0.1:9").unwrap())
            .build()
            .unwrap();
        let rt = crate::runtime::build_runtime().expect("tokio runtime");

        let competitor = Competitor::start(&reader);
        let failures: Vec<String> = competed_rounds(|| {
            let rows = rt
                .block_on(crate::modules::pcsc_list::list_cards(&client))
                .expect("list_cards failed");
            let row = rows
                .into_iter()
                .find(|r| r.reader == reader_name)
                .expect("the reader is missing from the listing");
            (row.imsi.as_deref() != Some(imsi.as_str()) || !row.status.starts_with("ok"))
                .then_some(row.status)
        })
        .into_iter()
        .flatten()
        .collect();
        let competing_commands = competitor.stop();

        assert!(
            competing_commands >= COMPETED_ROUNDS,
            "the competitor barely ran ({competing_commands} commands), so this proved nothing"
        );
        assert!(
            failures.is_empty(),
            "{} of {COMPETED_ROUNDS} listings were interleaved; first: {}",
            failures.len(),
            failures[0]
        );
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
    fn authenticate_usim_reaches_the_card_and_leaves_it() {
        let holder_ctx = context();
        let reader = first_real_reader(&holder_ctx);

        let imsi = card_imsi(&holder_ctx, &reader);

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
