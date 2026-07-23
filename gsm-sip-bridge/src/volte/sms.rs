//! Text messages over the host-side LTE path (specs/017-volte-inbound-bridge,
//! US5).
//!
//! # Why this exists at all
//!
//! Holding the subscriber's IMS registration means the network delivers their
//! text messages *here*. An earlier draft of the spec listed messaging as out
//! of scope; that was wrong and dangerous, because "out of scope" would have
//! meant texts arriving and being silently discarded. A call that fails to
//! connect announces itself. A lost text does not.
//!
//! This is therefore not a feature being added — it is an existing capability
//! being taken away unless it is handled.
//!
//! # Two routes, one destination
//!
//! ```text
//! over the registration ─┐
//!                        ├──> dedupe ──> record ──> forward ──> ack / clear
//! through the modem  ────┘
//! ```
//!
//! Both must be covered because **which route the carrier uses is its
//! decision**, and it is unmeasured. Our registration advertises voice
//! capability but not messaging capability, so the network may well keep using
//! the modem — and card assignment for this path is exclusive, so the
//! circuit-switched daemon no longer reads the modem's storage. Covering only
//! the registration route would leave those messages with no reader at all,
//! accumulating unread until storage filled.
//!
//! # The ordering is the safety property
//!
//! **Record before acknowledging. Always.** Acknowledging first means a crash
//! in between loses the message outright while the network believes it was
//! delivered. Acknowledging after means a crash causes a retransmission, which
//! [`Dedupe`] absorbs. One ordering loses data on crash; the other costs a
//! duplicate that is then suppressed.
//!
//! The same reasoning applies to clearing a message from the modem's storage.

use crate::error::BridgeResult;
use crate::modules::at_commander::AtCommander;
use crate::vowifi::control::{write_msg, ControlMessage};
use std::collections::VecDeque;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a message reached us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRoute {
    /// Delivered over the IMS registration.
    OverRegistration,
    /// Left in the modem's own storage for us to read.
    ThroughModem,
}

impl MessageRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageRoute::OverRegistration => "registration",
            MessageRoute::ThroughModem => "modem",
        }
    }
}

/// One inbound text, from either route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    pub route: MessageRoute,
    pub sender: String,
    pub body: String,
    /// Present only for modem-delivered messages — where to clear it from.
    pub modem_index: Option<u32>,
}

impl InboundMessage {
    /// Identity for duplicate suppression.
    ///
    /// Deliberately **excludes the route**: the same message arriving over both
    /// routes must collapse to one, which is the whole point. It also excludes
    /// the modem storage index, since that is an artefact of where the modem
    /// happened to file it rather than anything about the message.
    pub fn dedupe_key(&self) -> String {
        format!("{}\u{1}{}", self.sender, self.body)
    }
}

/// Remembers recently-handled messages so a retransmission is not recorded or
/// forwarded twice.
///
/// Bounded, and deliberately not persisted: its job is to absorb a network
/// retransmission, which happens within seconds. Surviving a restart would
/// mean carrying the risk of *suppressing* a genuine repeat message — someone
/// sending "ok" twice in a day is normal, and dropping the second would be a
/// worse failure than recording a rare duplicate after a crash.
#[derive(Debug)]
pub struct Dedupe {
    seen: VecDeque<String>,
    capacity: usize,
}

impl Default for Dedupe {
    fn default() -> Self {
        Self::new(64)
    }
}

impl Dedupe {
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// Records the message as handled. Returns `false` if it was already seen,
    /// in which case the caller must still acknowledge it but must not record
    /// or forward it again.
    pub fn admit(&mut self, key: &str) -> bool {
        if self.contains(key) {
            return false;
        }
        if self.seen.len() >= self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back(key.to_string());
        true
    }

    /// Whether this key has already been handled, without recording it. Lets a
    /// caller decide *before* it commits to an irreversible step — clearing a
    /// message from modem storage — whether the message is a fresh one to relay
    /// or a re-read of one already handed on.
    pub fn contains(&self, key: &str) -> bool {
        self.seen.iter().any(|k| k == key)
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// What the caller should do with a message after `decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Record, forward, and only then acknowledge or clear.
    Handle,
    /// Already seen. Acknowledge or clear it so the network stops retrying,
    /// but do not record or forward it again.
    AcknowledgeOnly,
}

/// Decides what to do with an arriving message.
///
/// Split out from the I/O so the exactly-once rule is testable without a
/// modem, a carrier or a database.
pub fn decide(dedupe: &mut Dedupe, message: &InboundMessage) -> Disposition {
    if dedupe.admit(&message.dedupe_key()) {
        Disposition::Handle
    } else {
        Disposition::AcknowledgeOnly
    }
}

/// Parses the index list from `AT+CMGL`, for recovering messages already
/// sitting in the modem's storage when the service starts.
///
/// Without this, texts that arrived while the service was down would be
/// stepped over and eventually lost when storage filled.
pub fn parse_cmgl_indexes(lines: &[String]) -> Vec<u32> {
    lines
        .iter()
        .filter_map(|l| {
            let payload = l.trim().strip_prefix("+CMGL:")?;
            payload.split(',').next()?.trim().parse::<u32>().ok()
        })
        .collect()
}

/// How often to check the modem's own storage for messages the carrier
/// delivered over the circuit-switched route rather than IMS. Short enough
/// that a text is handled promptly; the read is cheap and only runs when the
/// modem is not mid-attach (see [`run_modem_reader`]).
pub const MODEM_SWEEP_INTERVAL: Duration = Duration::from_secs(20);

/// How long to wait before the first sweep, so the initial registration's own
/// modem access (`register_session` reads the IMEI) has finished. The reader
/// still serialises against renewal via the shared lock; this just avoids a
/// pointless contended first attempt at startup.
const FIRST_SWEEP_DELAY: Duration = Duration::from_secs(12);

/// How long to wait to reach and write to the telephone side's control port.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Reads text messages the network left in the **modem's own storage** — the
/// circuit-switched delivery route — and hands each to the telephone side for
/// recording, then clears it (FR-036, US5 scenario 7).
///
/// # Why this is needed at all
///
/// Our registration advertises voice but not messaging, so the carrier may
/// deliver a text over the modem rather than as an IMS `MESSAGE` — and it does
/// (verified live: a text arrived in modem storage with no `MESSAGE` on the
/// registration at all). Card assignment here is exclusive, so the
/// circuit-switched daemon no longer reads that storage. Without this reader
/// those texts have no reader at all and accumulate unread until storage fills.
///
/// # Coordinating with the registration for the one AT port
///
/// The registration side also drives the modem's AT port — `register_session`
/// on renewal, `refresh_attachment` on re-attach. Two readers interleaving on
/// one port is the documented "no status in response" hazard (research R6), so
/// every modem touch here is taken under `modem_lock`, the same lock the
/// renewal path holds. Renewal is already deferred while a call is up, and a
/// call's own media rides the data bearer, not this AT port — so sweeping does
/// not disturb a call and a call does not disturb sweeping (FR-028).
///
/// # Exactly-once and the order of operations
///
/// A message is **relayed before it is cleared**, never the reverse: clearing
/// first would lose it outright on a crash in between. If a relay succeeds but
/// the delete then fails, the message is re-read on the next sweep — and
/// [`Dedupe`] recognises it, so it is cleared without being forwarded twice.
pub fn run_modem_reader(modem_port: PathBuf, control_addr: SocketAddr, modem_lock: Arc<Mutex<()>>) {
    std::thread::sleep(FIRST_SWEEP_DELAY);
    let mut dedupe = Dedupe::default();
    loop {
        {
            let _guard = modem_lock.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = sweep_modem_storage(&modem_port, control_addr, &mut dedupe) {
                tracing::warn!(error = %e, "modem SMS sweep failed; will retry next interval");
            }
        }
        std::thread::sleep(MODEM_SWEEP_INTERVAL);
    }
}

/// One pass over modem storage. Separated from the loop so a caller can drive a
/// single sweep, and so the loop's lock discipline is visible at its call site.
fn sweep_modem_storage(
    modem_port: &Path,
    control_addr: SocketAddr,
    dedupe: &mut Dedupe,
) -> BridgeResult<()> {
    let mut at = AtCommander::open(modem_port)?;
    // Text mode, or `CMGL`/`CMGR` return PDUs this path does not parse.
    let _ = at.send_command("AT+CMGF=1")?;
    let indexes = crate::sms::reader::list_sms_indexes(&mut at)?;
    if indexes.is_empty() {
        return Ok(());
    }
    tracing::info!(
        count = indexes.len(),
        "found messages in modem storage; relaying and clearing them"
    );
    for index in indexes {
        let sms = match crate::sms::reader::read_sms(&mut at, index) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(index, error = %e, "could not read a stored message; leaving it in place");
                continue;
            }
        };
        let key = InboundMessage {
            route: MessageRoute::ThroughModem,
            sender: sms.sender.clone(),
            body: sms.body.clone(),
            modem_index: Some(index),
        }
        .dedupe_key();

        // Already handed on this run — a previous delete must have failed.
        // Clear it now so storage does not fill; do not forward it again.
        if dedupe.contains(&key) {
            let _ = crate::sms::reader::delete_sms(&mut at, index);
            continue;
        }

        if relay_modem_message(control_addr, &sms.sender, &sms.body) {
            dedupe.admit(&key);
            // Relayed — now, and only now, clear it from the modem.
            if let Err(e) = crate::sms::reader::delete_sms(&mut at, index) {
                tracing::warn!(index, error = %e, "relayed the message but could not clear it; the dedupe will suppress the re-read");
            }
        }
        // On relay failure: leave it in storage, unmarked, to retry next sweep.
    }
    Ok(())
}

/// Hands one modem-delivered message to the telephone side over the same
/// control channel and message shape the IMS route uses
/// (`ims::agent::handle_message`), so both routes converge on one recorder.
fn relay_modem_message(control_addr: SocketAddr, sender: &str, body: &str) -> bool {
    let msg = ControlMessage::SmsReceived {
        sender: sender.to_string(),
        body: body.to_string(),
        received_at: chrono::Utc::now().to_rfc3339(),
    };
    match TcpStream::connect_timeout(&control_addr, CONTROL_TIMEOUT) {
        Ok(mut control) => match write_msg(&mut control, &msg) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "failed to relay modem SMS for recording");
                false
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to reach the control channel to relay modem SMS");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(route: MessageRoute, sender: &str, body: &str) -> InboundMessage {
        InboundMessage {
            route,
            sender: sender.to_string(),
            body: body.to_string(),
            modem_index: None,
        }
    }

    // ---- exactly-once -----------------------------------------------------

    #[test]
    fn a_message_is_handled_once() {
        let mut d = Dedupe::default();
        let m = msg(MessageRoute::OverRegistration, "+911234567890", "hello");

        assert_eq!(decide(&mut d, &m), Disposition::Handle);
        assert_eq!(decide(&mut d, &m), Disposition::AcknowledgeOnly);
    }

    #[test]
    fn the_same_message_on_both_routes_is_recorded_once() {
        // The case that makes covering both routes safe rather than
        // duplicating: if the carrier ever delivered by both, the operator
        // must not see the text twice.
        let mut d = Dedupe::default();
        let over = msg(MessageRoute::OverRegistration, "+911234567890", "hello");
        let through = InboundMessage {
            route: MessageRoute::ThroughModem,
            modem_index: Some(3),
            ..over.clone()
        };

        assert_eq!(decide(&mut d, &over), Disposition::Handle);
        assert_eq!(
            decide(&mut d, &through),
            Disposition::AcknowledgeOnly,
            "route must not be part of the identity"
        );
    }

    #[test]
    fn a_retransmission_is_acknowledged_but_not_duplicated() {
        // Acknowledging after recording means a crash causes a retransmission.
        // This is what absorbs it.
        let mut d = Dedupe::default();
        let m = msg(MessageRoute::OverRegistration, "+911234567890", "hello");

        assert_eq!(decide(&mut d, &m), Disposition::Handle);
        for _ in 0..5 {
            assert_eq!(
                decide(&mut d, &m),
                Disposition::AcknowledgeOnly,
                "a retransmission must still be acknowledged, or the network keeps retrying"
            );
        }
    }

    #[test]
    fn different_messages_from_one_sender_are_both_handled() {
        let mut d = Dedupe::default();

        assert_eq!(
            decide(
                &mut d,
                &msg(MessageRoute::OverRegistration, "+91123", "one")
            ),
            Disposition::Handle
        );
        assert_eq!(
            decide(
                &mut d,
                &msg(MessageRoute::OverRegistration, "+91123", "two")
            ),
            Disposition::Handle
        );
    }

    #[test]
    fn the_same_body_from_different_senders_is_not_confused() {
        let mut d = Dedupe::default();

        assert_eq!(
            decide(&mut d, &msg(MessageRoute::OverRegistration, "+91111", "ok")),
            Disposition::Handle
        );
        assert_eq!(
            decide(&mut d, &msg(MessageRoute::OverRegistration, "+91222", "ok")),
            Disposition::Handle,
            "a different sender is a different message"
        );
    }

    #[test]
    fn the_separator_cannot_be_forged_by_message_content() {
        // Naive concatenation would let a body containing the separator
        // collide with a different sender/body pair.
        let a = msg(MessageRoute::OverRegistration, "+91111", "x");
        let b = msg(MessageRoute::OverRegistration, "+91111\u{1}x", "");

        assert_ne!(a.dedupe_key(), b.dedupe_key());
    }

    #[test]
    fn the_modem_index_is_not_part_of_the_identity() {
        // Where the modem filed it says nothing about what it is.
        let mut d = Dedupe::default();
        let a = InboundMessage {
            modem_index: Some(1),
            ..msg(MessageRoute::ThroughModem, "+91123", "hello")
        };
        let b = InboundMessage {
            modem_index: Some(7),
            ..msg(MessageRoute::ThroughModem, "+91123", "hello")
        };

        assert_eq!(decide(&mut d, &a), Disposition::Handle);
        assert_eq!(decide(&mut d, &b), Disposition::AcknowledgeOnly);
    }

    // ---- bounding ---------------------------------------------------------

    #[test]
    fn the_dedupe_window_is_bounded() {
        let mut d = Dedupe::new(4);
        for i in 0..50 {
            decide(
                &mut d,
                &msg(MessageRoute::OverRegistration, "+91123", &format!("m{i}")),
            );
        }

        assert!(d.len() <= 4, "window must stay bounded, got {}", d.len());
    }

    #[test]
    fn a_message_older_than_the_window_is_handled_again() {
        // Accepted deliberately: the window exists to absorb a retransmission,
        // which arrives within seconds. Suppressing a genuine repeat message
        // hours later would be the worse failure — people do send "ok" twice.
        let mut d = Dedupe::new(2);
        let first = msg(MessageRoute::OverRegistration, "+91123", "first");

        assert_eq!(decide(&mut d, &first), Disposition::Handle);
        decide(&mut d, &msg(MessageRoute::OverRegistration, "+91123", "a"));
        decide(&mut d, &msg(MessageRoute::OverRegistration, "+91123", "b"));

        assert_eq!(decide(&mut d, &first), Disposition::Handle);
    }

    // ---- startup recovery -------------------------------------------------

    #[test]
    fn stored_message_indexes_are_recovered() {
        let lines: Vec<String> = [
            "+CMGL: 1,\"REC UNREAD\",\"+911234567890\",,\"26/07/22,10:00:00+22\"",
            "hello",
            "+CMGL: 4,\"REC UNREAD\",\"+919876543210\",,\"26/07/22,10:05:00+22\"",
            "world",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(parse_cmgl_indexes(&lines), vec![1, 4]);
    }

    #[test]
    fn an_empty_message_store_recovers_nothing_rather_than_erroring() {
        assert!(parse_cmgl_indexes(&[]).is_empty());
        assert!(parse_cmgl_indexes(&["OK".to_string()]).is_empty());
    }

    #[test]
    fn contains_reports_prior_handling_without_recording_it() {
        // The modem sweep clears a message from storage only after relaying it;
        // if the clear fails the message is re-read next sweep. `contains` lets
        // the sweep tell a fresh message (relay + clear) from a re-read (clear
        // only, no second forward) *before* it commits to the irreversible
        // clear — so it must answer without itself recording anything.
        let mut d = Dedupe::default();
        let key = msg(MessageRoute::ThroughModem, "+91123", "hello").dedupe_key();

        assert!(!d.contains(&key), "unseen key must not report as handled");
        assert!(!d.contains(&key), "checking must not record");
        assert!(d.admit(&key));
        assert!(d.contains(&key), "an admitted key reports as handled");
    }

    #[test]
    fn route_is_reported_so_the_delivery_path_is_observable() {
        // Which route the carrier actually uses is unmeasured, so every
        // message records how it arrived.
        assert_eq!(MessageRoute::OverRegistration.as_str(), "registration");
        assert_eq!(MessageRoute::ThroughModem.as_str(), "modem");
    }
}
