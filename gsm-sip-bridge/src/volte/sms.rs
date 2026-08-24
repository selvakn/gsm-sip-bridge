//! Text messages delivered to a line's own modem storage rather than over its
//! IMS registration. Despite the module path, this is no longer LTE-specific:
//! introduced for the host-side LTE path (specs/017-volte-inbound-bridge,
//! US5), and reused as-is by the VoWiFi agent (`ims::agent::run_inner`,
//! specs/038-reliable-sms-delivery) once the same gap was confirmed to exist
//! there too. It stays in `volte::sms` rather than moving to a
//! transport-neutral module — the logic is identical either way, and a move
//! is a larger change than the bug it would fix.
//!
//! # Why this exists at all
//!
//! Holding the subscriber's IMS registration means the network delivers their
//! text messages *here*. An earlier draft of the VoLTE spec listed messaging
//! as out of scope; that was wrong and dangerous, because "out of scope"
//! would have meant texts arriving and being silently discarded. A call that
//! fails to connect announces itself. A lost text does not.
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
use std::time::{Duration, Instant};

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
    /// Subset of `seen` known to have been *durably delivered*, not merely
    /// claimed (specs/038-reliable-sms-delivery review follow-up). `admit`
    /// alone cannot tell a caller "this claim already succeeded" from "this
    /// claim is still in flight and might yet fail" — and confusing the two
    /// is exactly what let a message be deleted from modem storage on the
    /// strength of a claim that later turned out to have failed. Populated
    /// only by `confirm`, which the caller must call once its relay actually
    /// succeeds; never inferred from admission or elapsed time.
    confirmed: std::collections::HashSet<String>,
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
            confirmed: std::collections::HashSet::new(),
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
            if let Some(evicted) = self.seen.pop_front() {
                self.confirmed.remove(&evicted);
            }
        }
        self.seen.push_back(key.to_string());
        true
    }

    /// Whether this key has already been handled, without recording it. Lets a
    /// caller decide *before* it commits to an irreversible step — clearing a
    /// message from modem storage — whether the message is a fresh one to relay
    /// or a re-read of one already handed on.
    ///
    /// This answers "has *anyone* claimed it", not "did that claim succeed" —
    /// for the latter, see `is_confirmed`. A caller about to do something
    /// irreversible (deleting the modem's only copy) on the strength of
    /// someone else's claim needs `is_confirmed`, not this.
    pub fn contains(&self, key: &str) -> bool {
        self.seen.iter().any(|k| k == key)
    }

    /// Whether `key`'s claim is known to have been *durably delivered* — set
    /// only by `confirm`, never inferred. A key can be `contains`-true and
    /// `is_confirmed`-false at the same time: claimed, outcome still pending.
    pub fn is_confirmed(&self, key: &str) -> bool {
        self.confirmed.contains(key)
    }

    /// Marks an existing claim as durably delivered. Call this — and only
    /// this — once a relay has actually succeeded; nothing else may treat a
    /// message as safe to irreversibly discard a backup copy of.
    pub fn confirm(&mut self, key: &str) {
        if self.contains(key) {
            self.confirmed.insert(key.to_string());
        }
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Reverses a prior `admit`. For a caller sharing one `Dedupe` across
    /// concurrent routes (specs/038-reliable-sms-delivery): admitting must
    /// happen *before* attempting the relay, not after, or two routes racing
    /// on the same message can both see "not yet admitted" and both relay it.
    /// But admitting before relaying means a relay that then fails has
    /// already claimed the key — `forget` releases that claim so the next
    /// attempt (a network retransmission, or the next modem sweep) is treated
    /// as fresh rather than permanently suppressed by a delivery that never
    /// actually happened. A failed relay was, by definition, never
    /// `confirm`ed, but this clears any confirmation anyway as a defensive
    /// no-op should a caller ever call both for the same key.
    pub fn forget(&mut self, key: &str) {
        self.seen.retain(|k| k != key);
        self.confirmed.remove(key);
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
/// on renewal, `refresh_attachment` on re-attach — and, on a VoWiFi line
/// (specs/038-reliable-sms-delivery), a wholly separate OS process
/// (`vowifi-usim-bridge`) drives it too for EAP-SIM's `AT+CSIM` traffic. Two
/// readers interleaving on one port is the documented "no status in response"
/// hazard (research R6). `modem_lock` only ever serialises threads *within*
/// this process — nothing at that level protects against `vowifi-usim-bridge`.
/// Cross-process exclusion doesn't need anything from this crate, though:
/// `AtCommander::open` opens through the `serialport` crate's default
/// exclusive mode, which already takes `TIOCEXCL` *and* a non-blocking
/// exclusive `flock` on the device path itself (confirmed in `serialport`
/// 4.9.0's source) — a concurrent holder is rejected immediately, the same
/// "fail fast and tolerate it" shape every caller here (including this one)
/// already handles by retrying later. `modem_lock` is still taken first, for
/// ordering against this *process's own* registration/renewal specifically.
///
/// Renewal is already deferred while a call is up, and a call's own media
/// rides the data bearer, not this AT port — so sweeping does not disturb a
/// call and a call does not disturb sweeping (FR-028).
pub fn run_modem_reader(
    modem_port: PathBuf,
    control_addr: SocketAddr,
    modem_lock: Arc<crate::modules::modem_lock::ModemLock>,
    dedupe: Arc<Mutex<Dedupe>>,
) {
    // This thread is detached and, before specs/039-at-stall-watchdog, entirely
    // unwatched — yet it is now the *most frequent* user of the AT port (every
    // 20s, versus an hourly renewal), and it holds `modem_lock` while it works.
    // A sweep that wedges therefore takes the whole line down with it: the next
    // renewal blocks forever acquiring that lock. Registering here means such a
    // sweep is caught in its own right, in seconds, rather than only when a
    // renewal eventually piles up behind it.
    let progress = crate::ims::agent::watchdog::register(Arc::new(
        crate::ims::agent::watchdog::Progress::new("sms-sweep"),
    ));
    // `Dormant`, not `Idle`, for both waits below. This thread is *supposed* to
    // be asleep between passes, and `MODEM_SWEEP_INTERVAL` is longer than
    // `Idle`'s budget -- resting in `Idle` made the watchdog confirm a stall and
    // kill the agent every ~36 seconds. Caught on the live line.
    progress.enter(crate::ims::agent::watchdog::Phase::Dormant);
    std::thread::sleep(FIRST_SWEEP_DELAY);
    loop {
        {
            let _phase = progress.phase_guard(crate::ims::agent::watchdog::Phase::SmsSweep);
            if let Err(e) = sweep_modem_storage(&modem_port, control_addr, &modem_lock, &dedupe) {
                tracing::warn!(error = %e, "modem SMS sweep failed; will retry next interval");
            }
        }
        progress.enter(crate::ims::agent::watchdog::Phase::Dormant);
        std::thread::sleep(MODEM_SWEEP_INTERVAL);
    }
}

/// One pass over modem storage, in three phases so the AT port is held only
/// for actual AT round-trips, never across a network relay or the
/// cross-route wait below — both of which can take seconds, and holding the
/// port (and therefore `serialport`'s own exclusive lock on it — see
/// `run_modem_reader`'s docs) that long would starve `vowifi-usim-bridge`/
/// charon's own AT needs far beyond the "seconds-long transaction" duty
/// cycle the shared port's design assumes.
///
/// # Every individual AT round-trip re-opens the port, on purpose
///
/// A real backlog is not one message — the bug this feature exists to fix
/// was found with 12 pending, and storage can hold up to 100. Holding one
/// continuous session across `AT+CMGF` + `AT+CMGL` + N × `AT+CMGR` (an
/// earlier version of this function did) makes phase 1's single hold scale
/// with backlog size — for a dozen-plus messages, easily several seconds,
/// comfortably exceeding `vowifi-usim-bridge`'s own total retry budget for a
/// busy port (5 attempts, linear backoff, ~5s total — see `usim_bridge`'s
/// `try_open_with_backoff`/`OPEN_RETRY_ATTEMPTS`/`OPEN_RETRY_BASE_DELAY`).
/// That risks the exact failure this design most wants to avoid: USIM
/// power-on giving up and muting EAP-SIM APDUs, failing the whole
/// registration, over an SMS sweep.
///
/// So `modem_lock` is taken, and the port opened, fresh for `AT+CMGF`+
/// `AT+CMGL` (one brief unit — listing needs both), then again individually
/// for *each* `AT+CMGR` read and each `AT+CMGD` delete — releasing the port
/// between every single AT command, not just between phases. This bounds
/// the worst-case *continuous* hold to one AT round-trip regardless of how
/// large the backlog is, so `vowifi-usim-bridge` always has a gap to slot
/// into, while a full backlog still drains in one sweep pass (no per-pass
/// cap): draining more slowly in wall-clock terms (one extra port re-open
/// per message) is a cost worth paying for that guarantee.
///
/// Bounding hold time only helps once this side has the port; it does
/// nothing for the reverse — a genuinely busy port (`vowifi-usim-bridge`
/// mid-session) makes `open` fail immediately (`serialport`'s own exclusive
/// open, not a lock this code owns — see `AtCommander::open_with_timeout`'s
/// docs). Failing once and waiting a full `MODEM_SWEEP_INTERVAL` (~20s) for
/// the next attempt would leave stored SMS unread for that whole window over
/// what is usually a session lasting a few seconds, so every open here goes
/// through [`open_with_retry`] instead: a short, bounded, caller-local
/// backoff — symmetric with `usim_bridge`'s own retry for the identical
/// contention from its side, just budgeted smaller since this can run many
/// times per sweep pass.
///
/// # `dedupe` is admitted *before* relaying, not after
///
/// Locking `dedupe` for this whole function (as an early version of this code
/// did) would block the registration-route handler for the entire sweep,
/// including every message's blocking network relay — needless latency/risk
/// of the network's own retransmission timer firing for an unrelated message.
/// The alternative of checking `contains` early but only calling `admit`
/// after a successful relay is worse: it reopens exactly the race a shared
/// `Dedupe` exists to close — two routes can both see "not yet admitted"
/// while each other's relay is in flight, and both then relay it, straight to
/// a duplicate. So each message here takes the lock twice, briefly: once via
/// [`decide`] to admit-or-detect-duplicate atomically *before* the relay, and
/// again via [`Dedupe::forget`] only if the relay then fails, so a message
/// that never actually got through is not permanently treated as handled.
///
/// # A bare "claimed elsewhere" is not "delivered elsewhere"
///
/// `decide` returning `AcknowledgeOnly` only means some other attempt admitted
/// this key first — not that it succeeded. That other attempt's relay may
/// still be in flight, or may fail and roll back a moment later, and a
/// retransmission may re-claim it yet again after that. A fixed "wait once,
/// then trust whatever `decide` says now" is not enough: a re-claim that
/// lands late in the wait window resets the clock without this function
/// knowing, so a bare re-check can still observe "claimed" for an attempt
/// that has barely started and may yet fail — deleting the modem's only copy
/// on that basis would silently lose the message.
///
/// So this never infers success from elapsed time. It polls
/// [`Dedupe::is_confirmed`] — set only by [`Dedupe::confirm`], which a
/// claimant calls only once its relay has actually succeeded — up to
/// [`CROSS_ROUTE_SETTLE_TIMEOUT`], checking every [`CROSS_ROUTE_POLL_INTERVAL`]:
/// confirmed at any point means genuinely delivered (safe to clear); the
/// claim disappearing (forgotten — rolled back, nothing re-claimed it since)
/// means this route delivers it itself instead of discarding the only copy;
/// timing out still claimed-but-unconfirmed leaves the message untouched for
/// the next sweep pass to re-evaluate from scratch, rather than guessing.
/// How many times [`open_with_retry`] will retry a busy port before giving
/// up, and the linear backoff step between attempts — the same shape as
/// `vowifi_usim_bridge`'s own `try_open_with_backoff`/`OPEN_RETRY_ATTEMPTS`/
/// `OPEN_RETRY_BASE_DELAY`, so the two sides of this exact contention treat
/// each other symmetrically rather than one waiting on the other's terms.
/// ~1.8s worst case (300 + 600 + 900ms) per open, not the same numbers as
/// `usim_bridge`'s ~5s: this runs per *individual* AT command inside a
/// sweep, potentially many times per pass, so its budget stays smaller to
/// avoid one contended message stalling an entire backlog drain, while
/// still meaningfully outlasting a brief collision instead of failing on
/// the first try and waiting a full `MODEM_SWEEP_INTERVAL` for the next.
const OPEN_RETRY_ATTEMPTS: u32 = 4;
const OPEN_RETRY_BASE_DELAY: Duration = Duration::from_millis(300);

/// Opens the modem port, retrying a busy-port failure with linear backoff
/// before giving up — see the constants above for why, and
/// `modules::at_commander::AtCommander::open_with_timeout`'s docs for why
/// this is a caller-local retry rather than something built into `open`
/// itself (discovery's fast-fail-across-many-candidates need would be hurt
/// by baking retries into every caller unconditionally).
fn open_with_retry(modem_port: &Path) -> BridgeResult<AtCommander> {
    let mut last_err = None;
    for attempt in 0..OPEN_RETRY_ATTEMPTS {
        match AtCommander::open(modem_port) {
            Ok(at) => return Ok(at),
            Err(e) => {
                last_err = Some(e);
                if attempt + 1 < OPEN_RETRY_ATTEMPTS {
                    std::thread::sleep(OPEN_RETRY_BASE_DELAY * (attempt + 1));
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once"))
}

/// How long one sweep pass may run before it abandons the rest of its work.
///
/// A pass has to be bounded because its *phase* is: the stall watchdog judges
/// `Phase::SmsSweep` against a fixed budget, and a pass that outruns it is killed
/// as a stall even though every individual step behaved correctly. That is easy
/// to reach — each per-message step can legitimately wait the full
/// `MODEM_LOCK_TIMEOUT` for a renewal to finish with the modem, so a handful of
/// stored messages is enough on its own.
///
/// Abandoning the remainder is cheap and safe, because a pass is idempotent by
/// design: unread messages stay in modem storage, relayed-but-undeleted ones are
/// suppressed by the dedupe, and the next pass is only
/// `MODEM_SWEEP_INTERVAL` away. Pinned against the watchdog budget by
/// [`tests::a_sweep_pass_cannot_outrun_its_watchdog_budget`].
const SWEEP_PASS_BUDGET: Duration = Duration::from_secs(60);

/// Worst case for one message's port work (read, or delete): waiting for the
/// modem, opening it, then one AT round trip.
const PER_MESSAGE_PORT_BUDGET: Duration = Duration::from_secs(30);

/// Worst case for relaying one message: a cross-route settle followed by one
/// relay attempt. No AT port involved.
const PER_MESSAGE_RELAY_BUDGET: Duration = Duration::from_secs(15);

/// The wall-clock bound on one sweep pass.
///
/// Only ever consulted *between* units of work — a pass never interrupts an AT
/// round trip half-way, it just declines to start another one it cannot finish.
struct PassBudget {
    deadline: Instant,
}

impl PassBudget {
    fn start() -> Self {
        Self {
            deadline: Instant::now() + SWEEP_PASS_BUDGET,
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Whether there is time to start a unit of work that could take `need`.
    fn room_for(&self, need: Duration) -> bool {
        self.remaining() >= need
    }

    /// How long to wait for the modem: the usual timeout, but never past this
    /// pass's own deadline.
    fn lock_wait(&self) -> Duration {
        self.remaining()
            .min(crate::modules::modem_lock::MODEM_LOCK_TIMEOUT)
    }
}

fn sweep_modem_storage(
    modem_port: &Path,
    control_addr: SocketAddr,
    modem_lock: &Arc<crate::modules::modem_lock::ModemLock>,
    dedupe: &Arc<Mutex<Dedupe>>,
) -> BridgeResult<()> {
    let budget = PassBudget::start();

    // Phase 1a: text mode + list indexes — one brief, combined session.
    let indexes = {
        // Bounded (specs/039-at-stall-watchdog): skipping this pass and
        // retrying in 20s is strictly better than joining a queue behind a
        // wedged holder, which is how one stuck activity used to become
        // several.
        let Some(_guard) = modem_lock.lock_timeout(budget.lock_wait()) else {
            return Err(crate::error::BridgeError::Discovery(
                "the modem is held by another user beyond the timeout; skipping this sweep".into(),
            ));
        };
        let mut at = open_with_retry(modem_port)?;
        // Text mode, or `CMGL`/`CMGR` return PDUs this path does not parse.
        let _ = at.send_command("AT+CMGF=1")?;
        crate::sms::reader::list_sms_indexes(&mut at)?
    };
    if indexes.is_empty() {
        return Ok(());
    }

    // Phase 1b: read each message in its own port session, so the port is
    // free between every read for vowifi-usim-bridge/charon to use if they
    // need it, regardless of how many messages are pending.
    let mut messages = Vec::with_capacity(indexes.len());
    for index in indexes {
        if !budget.room_for(PER_MESSAGE_PORT_BUDGET) {
            tracing::warn!(
                from_index = index,
                "sweep pass out of budget; leaving the remaining messages for the next pass"
            );
            break;
        }
        let sms = {
            match modem_lock.lock_timeout(budget.lock_wait()) {
                Some(_guard) => open_with_retry(modem_port)
                    .and_then(|mut at| crate::sms::reader::read_sms(&mut at, index)),
                None => Err(crate::error::BridgeError::Discovery(
                    "the modem is held by another user beyond the timeout".into(),
                )),
            }
        };
        match sms {
            Ok(sms) => messages.push(sms),
            Err(e) => {
                tracing::warn!(index, error = %e, "could not read a stored message; leaving it in place");
            }
        }
    }
    if messages.is_empty() {
        return Ok(());
    }
    tracing::info!(
        count = messages.len(),
        "found messages in modem storage; relaying and clearing them"
    );

    // Phase 2: decide and relay each message. No AT port held here.
    let mut to_delete = Vec::new();
    for sms in messages {
        // Checked *before* `decide` below, which claims the key: breaking after
        // a claim but before the relay would leave the key claimed and
        // unconfirmed, and the next pass would then read `AcknowledgeOnly` ->
        // `TimedOut` and skip the message forever. Breaking here claims nothing.
        if !budget.room_for(PER_MESSAGE_RELAY_BUDGET) {
            tracing::warn!(
                from_index = sms.index,
                "sweep pass out of budget; leaving the remaining messages unrelayed for the \
                 next pass"
            );
            break;
        }
        let inbound = InboundMessage {
            route: MessageRoute::ThroughModem,
            sender: sms.sender.clone(),
            body: sms.body.clone(),
            modem_index: Some(sms.index),
        };
        let key = inbound.dedupe_key();
        let disposition = {
            let mut d = dedupe.lock().unwrap_or_else(|e| e.into_inner());
            decide(&mut d, &inbound)
        };

        if disposition == Disposition::AcknowledgeOnly {
            match wait_for_resolution(dedupe, &key) {
                ClaimResolution::Confirmed => {
                    to_delete.push(sms.index);
                    continue;
                }
                ClaimResolution::Reclaimed => {
                    // The prior claim rolled back and `wait_for_resolution`
                    // has atomically re-claimed this key for us — fall
                    // through and relay it below.
                }
                ClaimResolution::TimedOut => {
                    // Still claimed, still unconfirmed, after outlasting
                    // every bound this codebase places on a relay attempt.
                    // Something is stuck rather than merely racing — leave
                    // the modem's copy untouched and let the next sweep
                    // pass, ~20s away, re-evaluate with a fresh look.
                    continue;
                }
            }
        }

        if relay_modem_message(control_addr, &sms.sender, &sms.body) {
            tracing::info!(
                index = sms.index,
                route = MessageRoute::ThroughModem.as_str(),
                "relayed a message found in modem storage"
            );
            dedupe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .confirm(&key);
            to_delete.push(sms.index);
        } else {
            // Leave it in storage, unmarked, to retry next sweep — and release
            // the admission above so that retry is not mistaken for a repeat.
            dedupe
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .forget(&key);
        }
    }

    // Phase 3: clear whatever is now confirmed handled — again, one port
    // session per message, for the same reason as phase 1b.
    for index in to_delete {
        if !budget.room_for(PER_MESSAGE_PORT_BUDGET) {
            // Safe to abandon: these were relayed and confirmed, so the dedupe
            // suppresses the re-read next pass, which will clear them then.
            tracing::warn!(
                from_index = index,
                "sweep pass out of budget; the dedupe will suppress these until the next pass \
                 clears them"
            );
            break;
        }
        let result = {
            match modem_lock.lock_timeout(budget.lock_wait()) {
                Some(_guard) => open_with_retry(modem_port)
                    .and_then(|mut at| crate::sms::reader::delete_sms(&mut at, index)),
                None => Err(crate::error::BridgeError::Discovery(
                    "the modem is held by another user beyond the timeout".into(),
                )),
            }
        };
        if let Err(e) = result {
            tracing::warn!(index, error = %e, "relayed the message but could not clear it; the dedupe will suppress the re-read");
        }
    }
    Ok(())
}

/// How often [`wait_for_resolution`] polls while a claim it does not own is
/// pending.
const CROSS_ROUTE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Total time [`wait_for_resolution`] will wait for a pending claim to
/// resolve before giving up. Comfortably longer than [`CONTROL_TIMEOUT`] (the
/// longest a single relay attempt can run), so an attempt that starts even
/// right before this budget begins still has time to finish within it.
const CROSS_ROUTE_SETTLE_TIMEOUT: Duration = Duration::from_secs(8);

/// How a claim this function did not make itself turned out.
enum ClaimResolution {
    /// The claim was confirmed delivered — someone's relay actually succeeded.
    Confirmed,
    /// The claim was rolled back and nothing has re-claimed it since. This
    /// call re-decided on the caller's behalf as part of observing that, so
    /// the key is now claimed *by this caller* — proceed to relay it.
    Reclaimed,
    /// Still claimed by someone else, outcome still unknown, after waiting
    /// the full budget. Left exactly as found; the caller must not act on it.
    TimedOut,
}

/// Polls a key's fate until it is confirmed, reclaimed, or the wait budget
/// runs out — never assumes success from elapsed time alone. See
/// `sweep_modem_storage`'s docs for why a single wait-then-recheck is not
/// enough: a retransmission racing into that window resets the clock without
/// the caller knowing, so only an explicit, caller-set confirmation signal
/// (never inferred) is trustworthy grounds for the irreversible modem delete
/// this exists to gate.
fn wait_for_resolution(dedupe: &Arc<Mutex<Dedupe>>, key: &str) -> ClaimResolution {
    let deadline = Instant::now() + CROSS_ROUTE_SETTLE_TIMEOUT;
    loop {
        {
            let mut d = dedupe.lock().unwrap_or_else(|e| e.into_inner());
            if d.is_confirmed(key) {
                return ClaimResolution::Confirmed;
            }
            if !d.contains(key) {
                // Rolled back and nothing has re-claimed it — claim it now,
                // atomically, so no window opens for a third party to sneak
                // in between this observation and the caller's relay attempt.
                d.admit(key);
                return ClaimResolution::Reclaimed;
            }
        }
        if Instant::now() >= deadline {
            return ClaimResolution::TimedOut;
        }
        std::thread::sleep(CROSS_ROUTE_POLL_INTERVAL);
    }
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

    #[test]
    fn a_sweep_pass_cannot_outrun_its_watchdog_budget() {
        // The bug this closes: nothing tied the pass's duration to the budget the
        // watchdog judges it against. Each per-message step can legitimately wait
        // the full `MODEM_LOCK_TIMEOUT` while a renewal holds the modem, so a
        // handful of stored messages pushed a *correctly behaving* pass past the
        // deadline and the watchdog killed a healthy line.
        let watchdog = crate::ims::agent::watchdog::Phase::SmsSweep
            .budget()
            .expect("the sweep is working, not resting, so it must carry a budget");
        assert!(
            SWEEP_PASS_BUDGET < watchdog,
            "a pass may run {SWEEP_PASS_BUDGET:?} but is judged against {watchdog:?}"
        );
        let margin = watchdog.as_secs_f64() / SWEEP_PASS_BUDGET.as_secs_f64() - 1.0;
        assert!(
            margin >= 0.20,
            "only {:.0}% headroom between the pass budget and the watchdog's; want >=20%",
            margin * 100.0
        );
    }

    #[test]
    fn the_listing_step_fits_inside_the_watchdogs_sweep_budget() {
        // Phase 1a is the one step a pass cannot decline: `PassBudget::room_for`
        // guards every *later* unit of work, but the lock, the open, `AT+CMGF`
        // and `AT+CMGL` all run before there is anything to guard. So its worst
        // case has to fit the watchdog's budget on its own, and `CMGL` now
        // carries a much larger deadline of its own than the port default it
        // used to inherit (`BULK_LIST_BUDGET`, added after a 208-message store
        // made every listing abort at 256 lines). Derived from the real
        // constants so raising any of them fails the build instead of arming a
        // false restart on a healthy line.
        let open_worst = OPEN_RETRY_BASE_DELAY * (1 + 2 + 3);
        // Excludes `at_commander::WORKER_GRACE` on each round trip, as the
        // sibling derivations here do; the margin asserted below covers it many
        // times over.
        let phase_1a_worst = crate::modules::modem_lock::MODEM_LOCK_TIMEOUT
            + open_worst
            + crate::modules::at_commander::DEFAULT_TIMEOUT
            + crate::sms::reader::BULK_LIST_BUDGET.timeout;
        let watchdog = crate::ims::agent::watchdog::Phase::SmsSweep
            .budget()
            .expect("the sweep is working, not resting, so it must carry a budget");
        assert!(
            watchdog > phase_1a_worst,
            "the listing step's worst case {phase_1a_worst:?} must fit the watchdog's \
             {watchdog:?}, since a pass cannot abandon it part-way"
        );
        let margin = watchdog.as_secs_f64() / phase_1a_worst.as_secs_f64() - 1.0;
        assert!(
            margin >= 0.20,
            "only {:.0}% headroom between the listing step and the watchdog's budget; want >=20%",
            margin * 100.0
        );
    }

    #[test]
    fn the_per_step_budgets_cover_their_worst_legitimate_case() {
        // Recomputed from the real constants, so raising any of them fails the
        // build rather than quietly letting a pass overrun again.
        let open_worst = OPEN_RETRY_BASE_DELAY * (1 + 2 + 3);
        let port_worst = crate::modules::modem_lock::MODEM_LOCK_TIMEOUT
            + open_worst
            + crate::modules::at_commander::DEFAULT_TIMEOUT;
        assert!(
            PER_MESSAGE_PORT_BUDGET >= port_worst,
            "{PER_MESSAGE_PORT_BUDGET:?} must cover one message's port work ({port_worst:?})"
        );

        let relay_worst = CROSS_ROUTE_SETTLE_TIMEOUT + CONTROL_TIMEOUT;
        assert!(
            PER_MESSAGE_RELAY_BUDGET >= relay_worst,
            "{PER_MESSAGE_RELAY_BUDGET:?} must cover one relay ({relay_worst:?})"
        );

        // And a pass must have room for at least the listing plus one message,
        // or it could make no progress at all and the queue would never drain.
        assert!(
            SWEEP_PASS_BUDGET >= PER_MESSAGE_PORT_BUDGET * 2,
            "a pass must fit the index listing and at least one message"
        );
    }

    #[test]
    fn an_exhausted_pass_declines_further_work_but_an_unstarted_one_allows_it() {
        let fresh = PassBudget::start();
        assert!(fresh.room_for(PER_MESSAGE_PORT_BUDGET));
        assert_eq!(
            fresh.lock_wait(),
            crate::modules::modem_lock::MODEM_LOCK_TIMEOUT,
            "a fresh pass should wait the ordinary modem timeout"
        );

        let spent = PassBudget {
            deadline: Instant::now(),
        };
        assert!(!spent.room_for(PER_MESSAGE_PORT_BUDGET));
        assert!(!spent.room_for(PER_MESSAGE_RELAY_BUDGET));
        assert_eq!(
            spent.lock_wait(),
            Duration::ZERO,
            "a spent pass must not start a fresh 20s wait for the modem"
        );

        // Part-spent: the lock wait is clipped to what is left, never extended.
        let nearly = PassBudget {
            deadline: Instant::now() + Duration::from_secs(3),
        };
        assert!(nearly.lock_wait() <= Duration::from_secs(3));
    }

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

    // ---- rollback on relay failure (specs/038-reliable-sms-delivery) ------

    #[test]
    fn forget_releases_an_admission_so_a_retry_is_treated_as_fresh() {
        let mut d = Dedupe::default();
        let key = msg(MessageRoute::OverRegistration, "+91123", "hello").dedupe_key();

        assert!(d.admit(&key));
        d.forget(&key);
        assert!(
            !d.contains(&key),
            "forget must fully release the admission, not just mark it stale"
        );
        assert!(
            d.admit(&key),
            "a forgotten key must be admittable again, exactly like a fresh one"
        );
    }

    /// The window `ims::agent::handle_message` withholds the SMS delivery
    /// report across. A second route seeing `AcknowledgeOnly` learns nothing
    /// about whether the first route's relay actually succeeded — and if it
    /// then fails, recovery is the network retransmitting. Reporting delivery
    /// on the strength of the claim alone would suppress exactly that retry,
    /// so `is_confirmed`, not `contains`, has to gate it.
    #[test]
    fn a_claim_can_be_acknowledge_only_while_still_unconfirmed() {
        let mut d = Dedupe::default();
        let inbound = msg(MessageRoute::OverRegistration, "+91123", "hello");
        let key = inbound.dedupe_key();

        assert_eq!(decide(&mut d, &inbound), Disposition::Handle);
        // The claimant's relay is still in flight here.
        assert_eq!(decide(&mut d, &inbound), Disposition::AcknowledgeOnly);
        assert!(d.contains(&key), "claimed");
        assert!(!d.is_confirmed(&key), "but the outcome is still pending");

        // That relay fails and releases the claim; the next arrival is fresh
        // again, which is only useful if the network was left free to retry.
        d.forget(&key);
        assert_eq!(decide(&mut d, &inbound), Disposition::Handle);

        // Once the claimant confirms, a later duplicate is safe to report.
        d.confirm(&key);
        assert_eq!(decide(&mut d, &inbound), Disposition::AcknowledgeOnly);
        assert!(d.is_confirmed(&key));
    }

    #[test]
    fn forgetting_an_unadmitted_key_is_a_harmless_no_op() {
        let mut d = Dedupe::default();
        let key = msg(MessageRoute::ThroughModem, "+91123", "hello").dedupe_key();
        d.forget(&key); // must not panic
        assert!(!d.contains(&key));
    }

    #[test]
    fn route_is_reported_so_the_delivery_path_is_observable() {
        // Which route the carrier actually uses is unmeasured, so every
        // message records how it arrived.
        assert_eq!(MessageRoute::OverRegistration.as_str(), "registration");
        assert_eq!(MessageRoute::ThroughModem.as_str(), "modem");
    }

    // ---- wait_for_resolution (specs/038 review follow-up) ------------------

    #[test]
    fn wait_for_resolution_returns_confirmed_once_the_claimant_confirms() {
        let dedupe = Arc::new(Mutex::new(Dedupe::default()));
        let key = "k1".to_string();
        dedupe.lock().unwrap().admit(&key);

        {
            let dedupe = dedupe.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                dedupe.lock().unwrap().confirm(&key);
            });
        }

        assert!(matches!(
            wait_for_resolution(&dedupe, &key),
            ClaimResolution::Confirmed
        ));
    }

    #[test]
    fn wait_for_resolution_reclaims_once_the_claim_is_forgotten() {
        let dedupe = Arc::new(Mutex::new(Dedupe::default()));
        let key = "k2".to_string();
        dedupe.lock().unwrap().admit(&key);

        {
            let dedupe = dedupe.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                dedupe.lock().unwrap().forget(&key);
            });
        }

        assert!(matches!(
            wait_for_resolution(&dedupe, &key),
            ClaimResolution::Reclaimed
        ));
        // The reclaim must have atomically re-admitted it for the caller.
        assert!(dedupe.lock().unwrap().contains(&key));
        assert!(!dedupe.lock().unwrap().is_confirmed(&key));
    }

    #[test]
    fn wait_for_resolution_times_out_on_a_claim_that_never_resolves() {
        let dedupe = Arc::new(Mutex::new(Dedupe::default()));
        let key = "k3".to_string();
        dedupe.lock().unwrap().admit(&key);
        // Nobody ever confirms or forgets it.

        let started = Instant::now();
        assert!(matches!(
            wait_for_resolution(&dedupe, &key),
            ClaimResolution::TimedOut
        ));
        assert!(started.elapsed() >= CROSS_ROUTE_SETTLE_TIMEOUT);
        // Left exactly as found: still claimed, still unconfirmed.
        assert!(dedupe.lock().unwrap().contains(&key));
        assert!(!dedupe.lock().unwrap().is_confirmed(&key));
    }

    // ---- open_with_retry (specs/038 review follow-up) ----------------------

    #[test]
    fn open_with_retry_gives_up_after_the_full_attempt_budget() {
        // A path that can never succeed exercises the retry *mechanics*
        // (attempt count, backoff) even though it's not a "busy port"
        // specifically — every attempt fails fast, so the elapsed time is
        // dominated by the backoff sleeps between attempts, which is exactly
        // what this asserts on.
        let bogus = Path::new("/nonexistent/gsm-sip-bridge-test-path");
        let started = Instant::now();
        assert!(open_with_retry(bogus).is_err());
        let expected_min: Duration = (1..OPEN_RETRY_ATTEMPTS)
            .map(|n| OPEN_RETRY_BASE_DELAY * n)
            .sum();
        assert!(
            started.elapsed() >= expected_min,
            "must actually wait out the backoff between attempts, not fail immediately"
        );
    }
}
