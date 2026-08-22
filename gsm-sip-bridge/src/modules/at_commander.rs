use crate::error::{BridgeError, BridgeResult};
// Re-exported so callers name the budget through the transport they already
// use, rather than reaching into the worker module directly.
pub use crate::modules::at_worker::ResponseBudget;
use std::fmt;
use std::io::{Read, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `pub(crate)` so `ims::agent::watchdog`'s budget-derivation test can recompute
/// the renewal worst case from the real constants rather than a copy of them.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// The most recent AT command this process issued, and when.
///
/// Recorded purely as a diagnostic for `ims::agent::watchdog`: when the
/// watchdog terminates a stalled line, "which AT command was it waiting on?" is
/// the first question anyone asks, and answering it in the log is the
/// difference between a five-second diagnosis and the live kernel-stack
/// forensics the 2026-08-16 incident needed.
///
/// A process-wide global rather than per-`AtCommander` state because agents run
/// one process per line, so there is exactly one interesting modem per process,
/// and threading a handle through every construction site would add parameters
/// to code paths that have no other reason to know about the watchdog.
static LAST_AT_COMMAND: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn record_last_at_command(cmd: &str) {
    let mut guard = LAST_AT_COMMAND.lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some((cmd.to_string(), Instant::now()));
}

/// The most recent AT command and when it was issued, if any.
pub(crate) fn last_at_command() -> Option<(String, Instant)> {
    LAST_AT_COMMAND
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}
const BAUD_RATE: u32 = 115200;

/// Matches `AT+CFUN=0` -> `AT+CFUN=1`'s settle time elsewhere in this
/// codebase (`supervise::sim_recovery::CFUN_CYCLE_DELAY`,
/// `vowifi::usim_bridge`'s in-place SIM reset) — the same modem-level
/// recipe, so the same wait.
const RADIO_CYCLE_DELAY: Duration = Duration::from_secs(4);

/// Slack added to the port's own timeout when waiting on the worker.
///
/// The worker applies the port timeout internally, so in normal operation it
/// always answers first and this outer wait never fires. It exists only to
/// catch the case the worker cannot report itself: its own `read(2)` never
/// returning.
const WORKER_GRACE: Duration = Duration::from_secs(2);

/// How the commands issued through this handle actually reach a modem.
enum Transport {
    /// A real serial port, owned by a worker thread
    /// (specs/039-at-stall-watchdog). The caller waits on a channel with its
    /// own deadline, so a `read(2)` that never returns costs a leaked worker
    /// rather than a frozen line.
    Worker {
        /// `None` between dropping a doomed worker's sender and installing a
        /// replacement — see `try_reopen`. A `None` here means no worker can be
        /// reached, which `dispatch` treats as dead.
        tx: Option<std::sync::mpsc::Sender<crate::modules::at_worker::Request>>,
        timeout: Duration,
        /// Set once a command has timed out. The next command resyncs before
        /// doing anything else; if that fails the channel is dead and every
        /// further command fails fast rather than queueing behind a worker
        /// that may never come back.
        state: ChannelState,
    },
    /// An in-memory stream, used only by tests.
    ///
    /// No worker: an in-memory `Read` cannot block on a syscall, so the whole
    /// reason the worker exists does not apply, and giving every scripted test
    /// a thread would buy nothing. Verified at the time of writing that all 26
    /// `from_stream` call sites are inside `#[cfg(test)]`.
    Direct(Box<crate::modules::at_worker::Session>),
}

/// Whether this handle's link to its worker is still usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelState {
    Healthy,
    /// A command timed out; try to resync before the next one.
    Suspect,
    /// Resync and reopen both failed — the worker is stuck holding the port.
    Dead,
}

pub struct AtCommander {
    transport: Transport,
    /// Kept so a dead channel can attempt a fresh open before giving up
    /// (FR-036). `None` for the in-memory test transport, which has no path.
    path: Option<std::path::PathBuf>,
}

pub trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

#[derive(Debug, Clone)]
pub enum AtResponse {
    Ok(Vec<String>),
    Error(String),
    CmeError(u32, String),
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Urc {
    Ring,
    Clip(String),
    Cmti { storage: String, index: u32 },
    NoCarrier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkType {
    FourGLte,
    ThreeGUmts,
    TwoGEdge,
    NoSignal,
    NoSim,
    Unknown,
}

impl fmt::Display for NetworkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkType::FourGLte => write!(f, "4G/LTE"),
            NetworkType::ThreeGUmts => write!(f, "3G/UMTS"),
            NetworkType::TwoGEdge => write!(f, "2G/EDGE"),
            NetworkType::NoSignal => write!(f, "No Signal"),
            NetworkType::NoSim => write!(f, "No SIM"),
            NetworkType::Unknown => write!(f, "Unknown"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    Auto,
    Gsm,
    Wcdma,
    Lte,
}

impl NetworkMode {
    pub fn at_value(self) -> u8 {
        match self {
            NetworkMode::Auto => 0,
            NetworkMode::Gsm => 1,
            NetworkMode::Wcdma => 2,
            NetworkMode::Lte => 3,
        }
    }

    pub fn from_at_value(v: u8) -> Option<Self> {
        match v {
            0 => Some(NetworkMode::Auto),
            1 => Some(NetworkMode::Gsm),
            2 => Some(NetworkMode::Wcdma),
            3 => Some(NetworkMode::Lte),
            _ => None,
        }
    }
}

impl fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetworkMode::Auto => write!(f, "auto"),
            NetworkMode::Gsm => write!(f, "2g"),
            NetworkMode::Wcdma => write!(f, "3g"),
            NetworkMode::Lte => write!(f, "4g"),
        }
    }
}

impl FromStr for NetworkMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(NetworkMode::Auto),
            "2g" | "gsm" => Ok(NetworkMode::Gsm),
            "3g" | "wcdma" => Ok(NetworkMode::Wcdma),
            "4g" | "lte" => Ok(NetworkMode::Lte),
            _ => Err(format!("unknown network mode: {s}")),
        }
    }
}

impl AtCommander {
    pub fn open(path: &Path) -> BridgeResult<Self> {
        Self::open_with_timeout(path, DEFAULT_TIMEOUT)
    }

    /// Like `open`, but with an explicit read timeout — used by
    /// `modules::discovery`'s AT-probe (specs/013-multi-card-vowifi FR-002),
    /// which tries several candidate serial interfaces per modem and wants a
    /// short per-candidate timeout rather than `DEFAULT_TIMEOUT`'s 5s.
    ///
    /// # Cross-process serialization (research/012 item 6) is already handled
    ///
    /// One physical AT port can have several independent OS processes wanting
    /// it — this half's own registration/renewal, `vowifi-usim-bridge`'s
    /// EAP-SIM APDUs over `AT+CSIM`, `modules::discovery`'s probes, and the
    /// modem SMS sweep (specs/038-reliable-sms-delivery). research/012 item 6
    /// named "an advisory `flock` on the device path inside `AtCommander::
    /// open`" as the escalation if this ever needed closing.
    ///
    /// An earlier version of this function did exactly that, with its own
    /// separate lock file handle. It was both redundant and actively broken:
    /// the `serialport` crate (confirmed in its source, v4.9.0) already opens
    /// exclusively by default — `TIOCEXCL` *and* a non-blocking exclusive
    /// `flock` on the device path itself, failing fast with a distinct,
    /// already-propagated error when another holder has it. Taking a second,
    /// separately-held lock on the same path before calling `serialport`'s
    /// own `open()` made *that* call's internal flock always fail (a process
    /// cannot hold two independently-acquired, conflicting flocks on one
    /// inode via different file descriptions — confirmed empirically) —
    /// every real-device open would have failed, always. Nothing here needs
    /// to add locking; `serialport` already provides it, correctly, for free.
    ///
    /// That still holds — but note it is *cross-process* exclusion only.
    /// Threads within this process contend through
    /// [`crate::modules::modem_lock::ModemLock`], and since
    /// specs/039-at-stall-watchdog the port itself is owned by a worker thread
    /// rather than by the returned handle: a `read(2)` that never comes back
    /// therefore strands that worker, and with it this `flock`, until the
    /// process exits. That is deliberate — see `modules::at_worker` for why
    /// bounding the caller is worth stranding the worker — but it means "the
    /// port is free again" is not something a surviving process can assume
    /// after a timeout. `AtCommander::ensure_usable` is what tries to find out.
    pub fn open_with_timeout(path: &Path, timeout: Duration) -> BridgeResult<Self> {
        let (tx, worker_rx) = std::sync::mpsc::channel();
        let session = Self::open_session(path, timeout)?;
        std::thread::Builder::new()
            .name(format!(
                "at-port-{}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "unknown".into())
            ))
            .spawn(move || crate::modules::at_worker::run(session, worker_rx))
            .map_err(|e| {
                BridgeError::Discovery(format!("could not start the AT port worker: {e}"))
            })?;
        Ok(Self {
            transport: Transport::Worker {
                tx: Some(tx),
                timeout,
                state: ChannelState::Healthy,
            },
            path: Some(path.to_path_buf()),
        })
    }

    fn open_session(
        path: &Path,
        timeout: Duration,
    ) -> BridgeResult<crate::modules::at_worker::Session> {
        let port = serialport::new(path.to_string_lossy(), BAUD_RATE)
            .timeout(timeout)
            .open()
            .map_err(|e| {
                BridgeError::Discovery(format!("failed to open serial {}: {e}", path.display()))
            })?;
        Ok(crate::modules::at_worker::Session::new(
            Box::new(port),
            timeout,
        ))
    }

    pub fn from_stream<S: Read + Write + Send + 'static>(stream: S, timeout: Duration) -> Self {
        Self {
            transport: Transport::Direct(Box::new(crate::modules::at_worker::Session::new(
                Box::new(stream),
                timeout,
            ))),
            path: None,
        }
    }

    pub fn reboot(&mut self) {
        // Fire-and-forget: modem will not send OK before it reboots
        self.send_command("AT+CFUN=1,1").ok();
    }

    /// Soft radio-cycle (`AT+CFUN=0` -> `AT+CFUN=1`): drops and re-acquires
    /// network registration without power-cycling the module or
    /// re-enumerating USB, unlike `reboot`'s `AT+CFUN=1,1` (which resets the
    /// whole module and can move its ttyUSB path). Fire-and-forget like
    /// `reboot`, for the same reason — the caller (a scheduled/manual
    /// restart) already treats the card as `Recovering` and waits for it to
    /// come back on its own, so there is nothing useful to do with either
    /// command's response here.
    pub fn radio_restart(&mut self) {
        self.send_command("AT+CFUN=0").ok();
        std::thread::sleep(RADIO_CYCLE_DELAY);
        self.send_command("AT+CFUN=1").ok();
    }

    pub fn send_command(&mut self, cmd: &str) -> BridgeResult<AtResponse> {
        self.send(cmd, None)
    }

    /// Like [`AtCommander::send_command`], but for a command whose legitimate
    /// response outgrows the port's default bounds — a bulk listing such as
    /// `AT+CMGL="ALL"`. See [`ResponseBudget`] for why those bounds are
    /// per-command, and `sms::reader::BULK_LIST_BUDGET` for the values.
    pub fn send_command_within(
        &mut self,
        cmd: &str,
        budget: ResponseBudget,
    ) -> BridgeResult<AtResponse> {
        self.send(cmd, Some(budget))
    }

    fn send(&mut self, cmd: &str, budget: Option<ResponseBudget>) -> BridgeResult<AtResponse> {
        record_last_at_command(cmd);
        match &mut self.transport {
            Transport::Direct(session) => match budget {
                Some(budget) => session.send_command_within(cmd, budget),
                None => session.send_command(cmd),
            },
            Transport::Worker { .. } => {
                self.ensure_usable()?;
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                self.dispatch(
                    crate::modules::at_worker::Request::Command {
                        cmd: cmd.to_string(),
                        budget,
                        reply: reply_tx,
                    },
                    reply_rx,
                    budget,
                )
            }
        }
    }

    /// Read one line, or fail on the deadline.
    ///
    /// The signature is unchanged for callers, but the *channel-state* decision
    /// underneath is not. A deadline reached with nothing buffered comes back
    /// from the worker as `Ok(None)` and leaves the channel `Healthy`; only then
    /// is it turned into the timeout error callers already handle. Treating an
    /// empty idle read as a worker error marked the channel `Suspect` on every
    /// poll of the circuit-switched URC loop, and the resync that followed
    /// discarded buffered bytes — silently dropping inbound `RING`/`+CMTI:`
    /// notifications, which is a strictly worse fault than the one being
    /// guarded against.
    pub fn read_line_raw(&mut self) -> BridgeResult<String> {
        let line = match &mut self.transport {
            Transport::Direct(session) => session.read_line_raw()?,
            Transport::Worker { .. } => {
                self.ensure_usable()?;
                let (reply_tx, reply_rx) = std::sync::mpsc::channel();
                self.dispatch(
                    crate::modules::at_worker::Request::ReadLine { reply: reply_tx },
                    reply_rx,
                    None,
                )?
            }
        };
        // Same error text as before, so `worker.rs`'s `contains("timeout")`
        // classification and every other caller keep working unchanged.
        line.ok_or_else(|| BridgeError::Discovery("AT read timeout".into()))
    }

    /// Hand a request to the worker and wait for it, bounded by our own
    /// deadline rather than the port's.
    ///
    /// The deadline is deliberately a little longer than the port's own
    /// timeout: the worker applies that timeout internally, so under normal
    /// operation it always answers first and this outer bound only fires when
    /// the worker itself is stuck in a syscall that will not return.
    ///
    /// `budget` is the one the worker was handed, so the two deadlines stay in
    /// step: a bulk listing that legitimately takes 30s must not be declared a
    /// stuck worker after the port's ordinary 5s.
    fn dispatch<T>(
        &mut self,
        request: crate::modules::at_worker::Request,
        reply_rx: std::sync::mpsc::Receiver<BridgeResult<T>>,
        budget: Option<ResponseBudget>,
    ) -> BridgeResult<T> {
        let Transport::Worker { tx, timeout, state } = &mut self.transport else {
            unreachable!("dispatch is only reached on the worker transport");
        };
        let wait = budget.map_or(*timeout, |b| b.timeout) + WORKER_GRACE;
        let Some(tx) = tx.as_ref() else {
            *state = ChannelState::Dead;
            return Err(BridgeError::Discovery(
                "the AT port has no worker; this line needs to be restarted".into(),
            ));
        };
        if tx.send(request).is_err() {
            *state = ChannelState::Dead;
            return Err(BridgeError::Discovery(
                "the AT port worker has stopped".into(),
            ));
        }
        match reply_rx.recv_timeout(wait) {
            Ok(result) => {
                if result.is_err() {
                    // The worker answered, but the command failed -- most
                    // likely its own timeout. Treat the channel as suspect so
                    // the next command resyncs rather than risking a stale
                    // reply being read as its answer.
                    *state = ChannelState::Suspect;
                }
                result
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                *state = ChannelState::Suspect;
                Err(BridgeError::Discovery("AT command timeout".into()))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                *state = ChannelState::Dead;
                Err(BridgeError::Discovery(
                    "the AT port worker has stopped".into(),
                ))
            }
        }
    }

    /// Bring a suspect channel back, or conclude it is dead (FR-036/FR-037).
    ///
    /// Cheapest remedy first. A resync costs one bare `AT` and rescues the
    /// common case where the worker was merely slow -- no restart, no
    /// interruption. Only if the worker will not answer at all do we try a
    /// fresh open, which succeeds if the stuck worker has since finished and
    /// dropped the port. If that fails too, the port is held by a worker that
    /// is never coming back, and nothing short of restarting this process will
    /// free it -- so the channel is marked dead and every subsequent command
    /// fails immediately instead of queueing behind it.
    fn ensure_usable(&mut self) -> BridgeResult<()> {
        let state = match &self.transport {
            Transport::Worker { state, .. } => *state,
            Transport::Direct(_) => return Ok(()),
        };
        match state {
            ChannelState::Healthy => Ok(()),
            ChannelState::Dead => Err(BridgeError::Discovery(
                "the AT port is held by an abandoned operation and cannot be used;                  this line needs to be restarted"
                    .into(),
            )),
            ChannelState::Suspect => {
                if self.try_resync() {
                    if let Transport::Worker { state, .. } = &mut self.transport {
                        *state = ChannelState::Healthy;
                    }
                    return Ok(());
                }
                if self.try_reopen() {
                    return Ok(());
                }
                if let Transport::Worker { state, .. } = &mut self.transport {
                    *state = ChannelState::Dead;
                }
                Err(BridgeError::Discovery(
                    "the AT port could not be resynchronised or reopened;                      this line needs to be restarted"
                        .into(),
                ))
            }
        }
    }

    fn try_resync(&mut self) -> bool {
        let (tx, timeout) = match &self.transport {
            Transport::Worker {
                tx: Some(tx),
                timeout,
                ..
            } => (tx.clone(), *timeout),
            // No worker to ask.
            Transport::Worker { tx: None, .. } => return false,
            Transport::Direct(_) => return true,
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if tx
            .send(crate::modules::at_worker::Request::Resync { reply: reply_tx })
            .is_err()
        {
            return false;
        }
        matches!(reply_rx.recv_timeout(timeout + WORKER_GRACE), Ok(Ok(())))
    }

    /// Try to take the port over with a brand-new worker.
    ///
    /// Dropping our sender first is what makes this reachable at all. A worker's
    /// `run` loop exits when every `Request` sender is gone, and only then does
    /// it drop the `Session` holding the port and its `flock`. While we kept the
    /// old `tx` alive, the old worker could never exit even after it had finished
    /// the command that made us suspicious, so `open_session` always failed with
    /// the port still held and this whole recovery path was dead code.
    ///
    /// It still cannot rescue a *genuinely* wedged worker: one parked in
    /// `read(2)` never returns to `recv()` and so never observes the closed
    /// channel. That case is what `ChannelState::Dead` and the process restart
    /// are for. What this rescues is the worker that was merely slow — it has
    /// since gone back to waiting, and now unwinds cleanly.
    fn try_reopen(&mut self) -> bool {
        let (Some(path), Transport::Worker { timeout, tx, .. }) =
            (self.path.clone(), &mut self.transport)
        else {
            return false;
        };
        let timeout = *timeout;
        // Release our end of the channel and let the old worker unwind.
        drop(tx.take());

        // The old worker has to be scheduled, notice the closed channel, and
        // drop its port before we can open it. Give it a bounded window rather
        // than one immediate attempt that would almost always lose that race.
        const REOPEN_ATTEMPTS: usize = 20;
        const REOPEN_RETRY_DELAY: Duration = Duration::from_millis(100);
        let mut session = None;
        for attempt in 0..REOPEN_ATTEMPTS {
            match Self::open_session(&path, timeout) {
                Ok(s) => {
                    session = Some(s);
                    break;
                }
                // Expected while the abandoned worker still holds the flock.
                Err(_) if attempt + 1 < REOPEN_ATTEMPTS => {
                    std::thread::sleep(REOPEN_RETRY_DELAY);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not reopen the modem port; the previous worker is still holding it"
                    );
                }
            }
        }
        let Some(session) = session else {
            return false;
        };
        let (new_tx, worker_rx) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name("at-port-reopen".to_string())
            .spawn(move || crate::modules::at_worker::run(session, worker_rx))
            .is_err()
        {
            return false;
        }
        tracing::warn!(
            path = %path.display(),
            "reopened the modem port after an abandoned AT operation; the previous worker              had released it"
        );
        self.transport = Transport::Worker {
            tx: Some(new_tx),
            timeout,
            state: ChannelState::Healthy,
        };
        true
    }

    pub fn check_signal(&mut self) -> BridgeResult<(u8, u8)> {
        match self.send_command("AT+CSQ")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(values) = line.strip_prefix("+CSQ: ") {
                        let parts: Vec<&str> = values.split(',').collect();
                        if parts.len() == 2 {
                            let rssi = parts[0].trim().parse().unwrap_or(99);
                            let ber = parts[1].trim().parse().unwrap_or(99);
                            return Ok((rssi, ber));
                        }
                    }
                }
                Err(BridgeError::Discovery("unexpected CSQ response".into()))
            }
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("CSQ failed: {e}")))
            }
        }
    }

    pub fn answer_call(&mut self) -> BridgeResult<()> {
        match self.send_command("ATA")? {
            AtResponse::Ok(_) => Ok(()),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("ATA failed: {e}")))
            }
        }
    }

    /// Places an outbound voice call (spec 025). `ATD{number};` — the
    /// trailing `;` is what keeps this a voice call rather than a data call
    /// (3GPP TS 27.007). Symmetric with `answer_call`/`ATA`: `OK` means the
    /// modem accepted the dial attempt and started it, not that it was
    /// answered — final call disposition (busy, no answer, rejected,
    /// answered) arrives later as unsolicited result codes, the same way
    /// `RING` does for an inbound call, not as this command's own response.
    pub fn dial(&mut self, number: &str) -> BridgeResult<()> {
        match self.send_command(&format!("ATD{number};"))? {
            AtResponse::Ok(_) => Ok(()),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("ATD failed: {e}")))
            }
        }
    }

    pub fn hangup(&mut self) -> BridgeResult<()> {
        match self.send_command("AT+CHUP")? {
            AtResponse::Ok(_) => Ok(()),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("CHUP failed: {e}")))
            }
        }
    }

    pub fn query_imei(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+CGSN")? {
            // Digit-only, like `query_imsi` below — not just "first non-empty
            // line". A modem with command echo enabled (the power-on default
            // on some firmware; nothing here ever sends ATE0) puts the
            // echoed "AT+CGSN" text itself as the first response line, and a
            // bare non-empty check happily returns that literal string as
            // the "IMEI" — sent on to the network inside +sip.instance and
            // silently accepted (found live: a real IMS REGISTER went out
            // with `+sip.instance="<urn:gsma:imei:AT+CGSN>"`). GSMA IMEIs are
            // 14-16 ASCII digits (14 + optional check digit, sometimes an SVN
            // suffix), so requiring digits-only rejects the echo the same
            // way `query_imsi`'s digit filter already does.
            AtResponse::Ok(lines) => lines
                .into_iter()
                .find(|l| l.chars().all(|c| c.is_ascii_digit()) && l.len() >= 14 && l.len() <= 16)
                .ok_or_else(|| BridgeError::Discovery("AT+CGSN: no IMEI in response".into())),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("AT+CGSN failed: {e}")))
            }
        }
    }

    pub fn query_imsi(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+CIMI")? {
            AtResponse::Ok(lines) => lines
                .into_iter()
                .find(|l| l.chars().all(|c| c.is_ascii_digit()) && l.len() >= 6)
                .ok_or_else(|| BridgeError::Discovery("AT+CIMI: no IMSI in response".into())),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("AT+CIMI failed: {e}")))
            }
        }
    }

    /// Raw `AT+CPIN?` status string (e.g. `"READY"`, `"SIM PIN"`, `"SIM
    /// PUK"`) — interpreting what that means for a line's usability is the
    /// caller's job (`modules::discovery::probe_sim_status`,
    /// specs/013-multi-card-vowifi FR-006).
    pub fn query_cpin(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+CPIN?")? {
            AtResponse::Ok(lines) => lines
                .into_iter()
                .find_map(|l| l.strip_prefix("+CPIN:").map(|s| s.trim().to_string()))
                .ok_or_else(|| BridgeError::Discovery("AT+CPIN?: no status in response".into())),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("AT+CPIN? failed: {e}")))
            }
        }
    }

    /// Number of MNC digits (2 or 3) in the home network's IMSI, read from
    /// the SIM's EF_AD administrative data file (3GPP TS 31.102 §4.2.18):
    /// `AT+CRSM=176,28589,0,0,4` is READ BINARY (176) of file 0x6FAD
    /// (28589), 4 bytes. On success the response is e.g.
    /// `+CRSM: 144,0,"00000002"` — sw1=144 (0x90, success) and the 4th
    /// octet's low nibble is the MNC length. Errors out (rather than
    /// guessing) when the byte is absent or invalid — legacy 2G SIMs may
    /// omit it entirely (TS 51.011 makes it optional), and callers are
    /// expected to fall back to `query_cops_plmn`.
    pub fn query_mnc_length(&mut self) -> BridgeResult<u8> {
        match self.send_command("AT+CRSM=176,28589,0,0,4")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix("+CRSM:") {
                        let parts: Vec<&str> = rest.splitn(3, ',').collect();
                        if parts.len() < 3 {
                            return Err(BridgeError::Discovery(format!(
                                "EF_AD read returned no data: +CRSM:{rest}"
                            )));
                        }
                        let sw1: u32 = parts[0].trim().parse().unwrap_or(0);
                        if sw1 != 144 {
                            return Err(BridgeError::Discovery(format!(
                                "EF_AD read rejected by the SIM: +CRSM:{rest}"
                            )));
                        }
                        let data = parts[2].trim().trim_matches('"');
                        let Some(byte4) = data.get(6..8) else {
                            return Err(BridgeError::Discovery(format!(
                                "EF_AD shorter than 4 bytes (no MNC length byte): {data:?}"
                            )));
                        };
                        let mnc_len = u8::from_str_radix(byte4, 16).map_err(|_| {
                            BridgeError::Discovery(format!("EF_AD byte 4 not hex: {data:?}"))
                        })? & 0x0F;
                        if mnc_len == 2 || mnc_len == 3 {
                            return Ok(mnc_len);
                        }
                        return Err(BridgeError::Discovery(format!(
                            "EF_AD MNC length not 2 or 3 (unprogrammed?): {mnc_len}"
                        )));
                    }
                }
                Err(BridgeError::Discovery(
                    "AT+CRSM: no +CRSM line in response".into(),
                ))
            }
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("AT+CRSM failed: {e}")))
            }
        }
    }

    /// The registered (serving) PLMN as the raw numeric MCC+MNC string —
    /// 5 or 6 digits, so the MNC digit count is unambiguous. Sets numeric
    /// operator format and reads it as one concatenated command
    /// (`AT+COPS=3,2;+COPS?` — a single response round trip) rather than
    /// two, so the format switch and the query can't interleave with
    /// another AT user. Response: `+COPS: 0,2,"40443",7`. Errors when not
    /// registered to a network (`+COPS: 0` carries no operator field).
    pub fn query_cops_plmn(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+COPS=3,2;+COPS?")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix("+COPS:") {
                        let parts: Vec<&str> = rest.splitn(4, ',').collect();
                        if parts.len() < 3 {
                            return Err(BridgeError::Discovery(format!(
                                "not registered to a network: +COPS:{rest}"
                            )));
                        }
                        let plmn = parts[2].trim().trim_matches('"');
                        if (plmn.len() == 5 || plmn.len() == 6)
                            && plmn.chars().all(|c| c.is_ascii_digit())
                        {
                            return Ok(plmn.to_string());
                        }
                        return Err(BridgeError::Discovery(format!(
                            "unexpected operator field in +COPS:{rest}"
                        )));
                    }
                }
                Err(BridgeError::Discovery(
                    "AT+COPS: no +COPS line in response".into(),
                ))
            }
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                Err(BridgeError::Discovery(format!("AT+COPS failed: {e}")))
            }
        }
    }

    pub fn query_phone_number(&mut self) -> BridgeResult<String> {
        match self.send_command("AT+CNUM")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix("+CNUM:") {
                        // +CNUM: "","+91XXXXXXXXXX",145
                        let parts: Vec<&str> = rest.splitn(3, ',').collect();
                        if parts.len() >= 2 {
                            let num = parts[1].trim().trim_matches('"');
                            if !num.is_empty() {
                                return Ok(num.to_string());
                            }
                        }
                    }
                }
                Ok("Unknown".to_string())
            }
            AtResponse::Error(_) | AtResponse::CmeError(_, _) => Ok("Unknown".to_string()),
        }
    }

    pub fn query_network_type(&mut self) -> BridgeResult<NetworkType> {
        match self.send_command("AT+QNWINFO")? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix("+QNWINFO:") {
                        // +QNWINFO: "FDD LTE","46001","LTE BAND 3",1825
                        let act = rest
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .trim_matches('"');
                        let nt = if act.contains("LTE") {
                            NetworkType::FourGLte
                        } else if act.contains("WCDMA")
                            || act.contains("UMTS")
                            || act.contains("HSPA")
                        {
                            NetworkType::ThreeGUmts
                        } else if act.contains("GSM")
                            || act.contains("GPRS")
                            || act.contains("EDGE")
                        {
                            NetworkType::TwoGEdge
                        } else {
                            NetworkType::NoSignal
                        };
                        return Ok(nt);
                    }
                }
                Ok(NetworkType::NoSignal)
            }
            AtResponse::Error(_) | AtResponse::CmeError(_, _) => Ok(NetworkType::NoSignal),
        }
    }

    pub fn query_network_mode(&mut self) -> BridgeResult<NetworkMode> {
        match self.send_command(r#"AT+QCFG="nwscanmode""#)? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.strip_prefix(r#"+QCFG: "nwscanmode","#) {
                        let val: u8 = rest
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("0")
                            .trim()
                            .parse()
                            .unwrap_or(0);
                        return Ok(NetworkMode::from_at_value(val).unwrap_or(NetworkMode::Auto));
                    }
                    // Some firmware omits the quotes around value:
                    if let Some(rest) = line.strip_prefix("+QCFG: \"nwscanmode\",") {
                        let val: u8 = rest
                            .trim()
                            .split(',')
                            .next()
                            .unwrap_or("0")
                            .trim()
                            .parse()
                            .unwrap_or(0);
                        return Ok(NetworkMode::from_at_value(val).unwrap_or(NetworkMode::Auto));
                    }
                }
                Ok(NetworkMode::Auto)
            }
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => Err(BridgeError::Discovery(
                format!("query network mode failed: {e}"),
            )),
        }
    }

    pub fn set_network_mode(&mut self, mode: NetworkMode) -> BridgeResult<NetworkMode> {
        let cmd = format!(r#"AT+QCFG="nwscanmode",{}"#, mode.at_value());
        match self.send_command(&cmd)? {
            AtResponse::Ok(_) => {}
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => {
                return Err(BridgeError::Discovery(format!(
                    "set network mode failed: {e}"
                )));
            }
        }
        // Verify the change took effect
        let confirmed = self.query_network_mode()?;
        if confirmed != mode {
            return Err(BridgeError::Discovery(format!(
                "network mode mismatch after set: expected {mode}, got {confirmed}"
            )));
        }
        Ok(confirmed)
    }

    /// The modem's `<ims_conf>` (`AT+QCFG="ims"` → `+QCFG: "ims",<ims_conf>,
    /// <volte_cap>`): 0 = follow the MBN default, 1 = forcibly enable IMS,
    /// 2 = forcibly disable it. Only `<ims_conf>` is returned — `<volte_cap>`
    /// is derived by the modem from it and is not separately settable.
    pub fn query_ims_conf(&mut self) -> BridgeResult<u8> {
        match self.send_command(r#"AT+QCFG="ims""#)? {
            AtResponse::Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.trim().strip_prefix(r#"+QCFG: "ims","#) {
                        return rest
                            .split(',')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .parse()
                            .map_err(|_| {
                                BridgeError::Discovery(format!(
                                    r#"AT+QCFG="ims": unparseable ims_conf in {line:?}"#
                                ))
                            });
                    }
                }
                Err(BridgeError::Discovery(
                    r#"AT+QCFG="ims": no +QCFG line in response"#.into(),
                ))
            }
            // Firmware without an IMS stack rejects the command outright.
            // That is a hard error, not "IMS is off": VoWiFi's correctness
            // depends on knowing the modem is not IMS-registered, and an
            // ERROR tells us nothing either way.
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => Err(BridgeError::Discovery(
                format!(r#"AT+QCFG="ims" failed: {e}"#),
            )),
        }
    }

    /// Sets `<ims_conf>`. The modem only applies it after a reboot
    /// (`AT+CFUN=1,1`) and persists it across power cycles, so the caller
    /// owns both the reboot and the re-verification — see
    /// `vowifi::ims_mode`.
    pub fn set_ims_conf(&mut self, ims_conf: u8) -> BridgeResult<()> {
        match self.send_command(&format!(r#"AT+QCFG="ims",{ims_conf}"#))? {
            AtResponse::Ok(_) => Ok(()),
            AtResponse::Error(e) | AtResponse::CmeError(_, e) => Err(BridgeError::Discovery(
                format!(r#"AT+QCFG="ims",{ims_conf} failed: {e}"#),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Mock stream: reads from a fixed byte buffer, discards writes.
    struct MockStream {
        reader: Cursor<Vec<u8>>,
    }

    impl MockStream {
        fn new(response: &str) -> Self {
            Self {
                reader: Cursor::new(response.as_bytes().to_vec()),
            }
        }
    }

    impl Read for MockStream {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reader.read(buf)
        }
    }

    impl Write for MockStream {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_commander(response: &str) -> AtCommander {
        AtCommander::from_stream(MockStream::new(response), Duration::from_secs(1))
    }

    #[test]
    fn dial_maps_ok_to_success() {
        let mut at = make_commander("OK\r\n");
        assert!(at.dial("+15551234567").is_ok());
    }

    #[test]
    fn dial_maps_error_to_failure() {
        let mut at = make_commander("ERROR\r\n");
        assert!(at.dial("15551234567").is_err());
    }

    #[test]
    fn dial_maps_cme_error_to_failure() {
        let mut at = make_commander("+CME ERROR: 30\r\n");
        assert!(at.dial("15551234567").is_err());
    }

    #[test]
    fn test_query_imei() {
        let mut at = make_commander("867584030123456\r\nOK\r\n");
        assert_eq!(at.query_imei().unwrap(), "867584030123456");
    }

    #[test]
    fn test_query_imei_skips_a_command_echo_line() {
        // A modem with command echo enabled (no ATE0 sent anywhere in this
        // codebase) puts the echoed "AT+CGSN" text as the first response
        // line — found live sending a real IMS REGISTER with
        // +sip.instance="<urn:gsma:imei:AT+CGSN>". The digit-only filter
        // must skip that line and find the real IMEI after it.
        let mut at = make_commander("AT+CGSN\r\n865396058758216\r\nOK\r\n");
        assert_eq!(at.query_imei().unwrap(), "865396058758216");
    }

    #[test]
    fn test_query_imei_rejects_a_too_short_digit_line() {
        // Not every all-digit line is a plausible IMEI — a stray short
        // numeric line (e.g. a status code on some other line) must not be
        // mistaken for one.
        let mut at = make_commander("123\r\n867584030123456\r\nOK\r\n");
        assert_eq!(at.query_imei().unwrap(), "867584030123456");
    }

    #[test]
    fn test_query_imsi() {
        let mut at = make_commander("404438083996440\r\nOK\r\n");
        assert_eq!(at.query_imsi().unwrap(), "404438083996440");
    }

    #[test]
    fn test_query_imsi_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(at.query_imsi().is_err());
    }

    #[test]
    fn test_query_cpin_ready() {
        let mut at = make_commander("+CPIN: READY\r\nOK\r\n");
        assert_eq!(at.query_cpin().unwrap(), "READY");
    }

    #[test]
    fn test_query_cpin_locked() {
        let mut at = make_commander("+CPIN: SIM PIN\r\nOK\r\n");
        assert_eq!(at.query_cpin().unwrap(), "SIM PIN");
    }

    #[test]
    fn test_query_cpin_error_no_sim() {
        let mut at = make_commander("+CME ERROR: 10\r\n");
        assert!(at.query_cpin().is_err());
    }

    #[test]
    fn test_query_phone_number_present() {
        let mut at = make_commander("+CNUM: \"\",\"+91XXXXXXXXXX\",145\r\nOK\r\n");
        assert_eq!(at.query_phone_number().unwrap(), "+91XXXXXXXXXX");
    }

    #[test]
    fn test_query_phone_number_error() {
        let mut at = make_commander("ERROR\r\n");
        assert_eq!(at.query_phone_number().unwrap(), "Unknown");
    }

    #[test]
    fn test_query_network_type_lte() {
        let mut at =
            make_commander("+QNWINFO: \"FDD LTE\",\"46001\",\"LTE BAND 3\",1825\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::FourGLte);
    }

    #[test]
    fn test_query_network_type_wcdma() {
        let mut at = make_commander("+QNWINFO: \"WCDMA\",\"46001\",\"WCDMA 850\",4400\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::ThreeGUmts);
    }

    #[test]
    fn test_query_network_type_gsm() {
        let mut at = make_commander("+QNWINFO: \"GSM\",\"46001\",\"GSM 900\",80\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::TwoGEdge);
    }

    #[test]
    fn test_query_network_type_no_signal() {
        let mut at = make_commander("ERROR\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::NoSignal);
    }

    #[test]
    fn test_query_network_mode() {
        let mut at = make_commander("+QCFG: \"nwscanmode\",3\r\nOK\r\n");
        assert_eq!(at.query_network_mode().unwrap(), NetworkMode::Lte);
    }

    #[test]
    fn test_network_mode_from_str() {
        assert_eq!("4g".parse::<NetworkMode>().unwrap(), NetworkMode::Lte);
        assert_eq!("2g".parse::<NetworkMode>().unwrap(), NetworkMode::Gsm);
        assert_eq!("3g".parse::<NetworkMode>().unwrap(), NetworkMode::Wcdma);
        assert_eq!("auto".parse::<NetworkMode>().unwrap(), NetworkMode::Auto);
        assert!("5g".parse::<NetworkMode>().is_err());
    }

    #[test]
    fn test_network_mode_display() {
        assert_eq!(NetworkMode::Lte.to_string(), "4g");
        assert_eq!(NetworkMode::Gsm.to_string(), "2g");
        assert_eq!(NetworkMode::Wcdma.to_string(), "3g");
        assert_eq!(NetworkMode::Auto.to_string(), "auto");
    }

    // Kills: NetworkType::fmt replaced with Ok(Default::default())
    #[test]
    fn test_network_type_display() {
        assert_eq!(NetworkType::FourGLte.to_string(), "4G/LTE");
        assert_eq!(NetworkType::ThreeGUmts.to_string(), "3G/UMTS");
        assert_eq!(NetworkType::TwoGEdge.to_string(), "2G/EDGE");
        assert_eq!(NetworkType::NoSignal.to_string(), "No Signal");
        assert_eq!(NetworkType::NoSim.to_string(), "No SIM");
        assert_eq!(NetworkType::Unknown.to_string(), "Unknown");
    }

    // Kills: NetworkMode::at_value returning wrong constant (0 or 1 for any variant)
    #[test]
    fn test_network_mode_at_value() {
        assert_eq!(NetworkMode::Auto.at_value(), 0);
        assert_eq!(NetworkMode::Gsm.at_value(), 1);
        assert_eq!(NetworkMode::Wcdma.at_value(), 2);
        assert_eq!(NetworkMode::Lte.at_value(), 3);
    }

    // Kills: NetworkMode::from_at_value match arms 0-3 deleted
    #[test]
    fn test_network_mode_from_at_value() {
        assert_eq!(NetworkMode::from_at_value(0), Some(NetworkMode::Auto));
        assert_eq!(NetworkMode::from_at_value(1), Some(NetworkMode::Gsm));
        assert_eq!(NetworkMode::from_at_value(2), Some(NetworkMode::Wcdma));
        assert_eq!(NetworkMode::from_at_value(3), Some(NetworkMode::Lte));
        assert_eq!(NetworkMode::from_at_value(4), None);
        assert_eq!(NetworkMode::from_at_value(255), None);
    }

    #[test]
    fn test_query_mnc_length_two_digits() {
        let mut at = make_commander("+CRSM: 144,0,\"00000002\"\r\nOK\r\n");
        assert_eq!(at.query_mnc_length().unwrap(), 2);
    }

    #[test]
    fn test_query_mnc_length_three_digits() {
        let mut at = make_commander("+CRSM: 144,0,\"01000003\"\r\nOK\r\n");
        assert_eq!(at.query_mnc_length().unwrap(), 3);
    }

    #[test]
    fn test_query_mnc_length_rejects_sim_error_status() {
        // sw1=106 (0x6A, file not found) — the SIM refused the read
        let mut at = make_commander("+CRSM: 106,130\r\nOK\r\n");
        assert!(at.query_mnc_length().is_err());
    }

    #[test]
    fn test_query_mnc_length_rejects_short_ef_ad() {
        // Legacy 2G SIM: EF_AD present but only 3 bytes, no MNC length byte
        let mut at = make_commander("+CRSM: 144,0,\"000000\"\r\nOK\r\n");
        assert!(at.query_mnc_length().is_err());
    }

    #[test]
    fn test_query_mnc_length_rejects_unprogrammed_value() {
        // 0xFF low nibble = 15 — neither 2 nor 3
        let mut at = make_commander("+CRSM: 144,0,\"000000FF\"\r\nOK\r\n");
        assert!(at.query_mnc_length().is_err());
    }

    #[test]
    fn test_query_mnc_length_at_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(at.query_mnc_length().is_err());
    }

    #[test]
    fn test_query_cops_plmn_five_digits() {
        let mut at = make_commander("+COPS: 0,2,\"40443\",7\r\nOK\r\n");
        assert_eq!(at.query_cops_plmn().unwrap(), "40443");
    }

    #[test]
    fn test_query_cops_plmn_six_digits() {
        let mut at = make_commander("+COPS: 0,2,\"405840\",7\r\nOK\r\n");
        assert_eq!(at.query_cops_plmn().unwrap(), "405840");
    }

    #[test]
    fn test_query_cops_plmn_not_registered() {
        // No operator field when unregistered
        let mut at = make_commander("+COPS: 0\r\nOK\r\n");
        assert!(at.query_cops_plmn().is_err());
    }

    #[test]
    fn test_query_cops_plmn_at_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(at.query_cops_plmn().is_err());
    }

    // Kills: || → && at line 296 (before UMTS) and 297 (before HSPA)
    #[test]
    fn test_query_network_type_umts_keyword() {
        let mut at = make_commander("+QNWINFO: \"UMTS\",\"46001\",\"UMTS 2100\",10812\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::ThreeGUmts);
    }

    #[test]
    fn test_query_network_type_hspa_keyword() {
        let mut at = make_commander("+QNWINFO: \"HSPA\",\"46001\",\"WCDMA 2100\",10812\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::ThreeGUmts);
    }

    // Kills: || → && at line 301 (before GPRS) and 302 (before EDGE)
    #[test]
    fn test_query_network_type_gprs_keyword() {
        let mut at = make_commander("+QNWINFO: \"GPRS\",\"46001\",\"GSM 900\",80\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::TwoGEdge);
    }

    #[test]
    fn test_query_network_type_edge_keyword() {
        let mut at = make_commander("+QNWINFO: \"EDGE\",\"46001\",\"GSM 900\",80\r\nOK\r\n");
        assert_eq!(at.query_network_type().unwrap(), NetworkType::TwoGEdge);
    }
}
