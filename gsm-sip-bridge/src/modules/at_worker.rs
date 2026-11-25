//! The thread that owns a real modem serial port, and the session logic that
//! drives it (specs/039-at-stall-watchdog).
//!
//! # Why a thread
//!
//! `serialport`'s read timeout is not a bound. `TTYPort::read` polls with the
//! timeout and then performs an **unguarded blocking `read(2)`** on a fd whose
//! `O_NONBLOCK` the crate deliberately clears — so if the tty's input buffer
//! empties between the poll and the read, the thread parks in `n_tty_read`
//! forever. That is exactly what took a production line down for 2h45m on
//! 2026-08-16.
//!
//! Bounding it from inside the calling thread is impossible: the fd cannot be
//! driven safely without `unsafe` (`TTYPort` exposes only `AsRawFd`), and this
//! crate is held at zero `unsafe` blocks. So the blocking call is moved
//! somewhere it can be abandoned — a worker thread — and the caller waits on a
//! channel with a deadline it controls.
//!
//! # What "abandoned" costs
//!
//! A wedged worker keeps its thread, its port handle and its `flock` for the
//! life of the process. That is the deliberate trade: the *caller* is bounded,
//! which keeps the line answering calls and lets the stall watchdog observe
//! progress, but full recovery of the port still needs the process to restart.
//! [`Session::sync`] exists to avoid paying that price whenever the worker
//! was merely slow rather than truly stuck.
//!
//! # Why the read buffer lives here, and why it is not enough
//!
//! It used to be a `BufReader` constructed per command and dropped on return,
//! which silently discarded whatever it had buffered past the terminating
//! `OK`. A reply arriving after a timeout was therefore read as the *next*
//! command's response, and every command after that was off by one —
//! permanently, with no error anywhere. Owning the buffer across commands
//! keeps what was read, but it cannot tell an abandoned reply from the next
//! one: a reply the modem is still producing arrives *after* the next write,
//! and a reply abandoned by another holder of the port arrives on the next
//! open (#237, measured on an EC200U). So nothing trusts the channel until
//! [`Session::sync`] has proved it is in step: on every open, and before the
//! next command after any timeout.

use crate::error::{BridgeError, BridgeResult};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::at_commander::{AtResponse, ReadWrite};

/// Default cap on lines accumulated for one response. A modem emitting
/// unsolicited output that never terminates must fail, not grow without limit
/// (FR-006).
///
/// A *default*, not an absolute: it is sized for the request/response commands
/// that make up almost all AT traffic, where anything past a handful of lines
/// is a runaway. Bulk listings are the documented exception — see
/// [`ResponseBudget`].
const MAX_RESPONSE_LINES: usize = 256;
/// Cap on unparsed bytes held for one response, for the same reason.
const MAX_BUFFERED_BYTES: usize = 64 * 1024;
/// The first of the two read-only queries a sync sends (research D2, F12).
///
/// The sync recognises its own replies by their `+CMEE:`/`+CSCS:` prefix, so
/// nothing else in the bridge may send either query: another sender's late
/// answer would be taken for the sync's. Both are standard TS 27.007 *read*
/// commands, so a sync writes no modem setting (FR-004) and needs no SIM.
const SYNC_FIRST: &str = "AT+CMEE?";
/// The second sync query, written only after the first one's reply. See
/// [`SYNC_FIRST`].
const SYNC_SECOND: &str = "AT+CSCS?";

/// What one command is allowed to spend before the channel is declared
/// runaway: how many lines its response may accumulate, and how long the whole
/// exchange may take.
///
/// # Why this is per-command rather than one global constant
///
/// The two bounds exist to catch a modem emitting output that never
/// terminates. Sized for ordinary request/response traffic that is easy — no
/// legitimate `AT+CSQ` runs to 256 lines or 5 seconds. But a handful of
/// commands are *bulk listings* whose correct, healthy response is far larger
/// than any plausible runaway, and `AT+CMGL="ALL"` is the one that bit: with
/// 208 messages in modem storage it returned 461 lines / 46,717 bytes
/// (measured on a live line, 2026-08-22), so every sweep aborted at line 256.
///
/// That failure is self-locking, which is what makes it worth a mechanism
/// rather than a bigger constant: the SMS sweep can only delete messages it
/// first managed to list, so once roughly 128 accumulate the listing always
/// overflows, storage can never drain, and it grows until the network can no
/// longer deliver to the card at all.
#[derive(Debug, Clone, Copy)]
pub struct ResponseBudget {
    /// Lines the response may accumulate before it is treated as runaway.
    pub max_lines: usize,
    /// Wall-clock bound on the whole exchange. Note this bounds *wire time*
    /// too, not just idleness: at 115200 8N1 a 46 KB response is already ~4s,
    /// so a bulk listing needs headroom here even when the line is perfectly
    /// healthy.
    pub timeout: Duration,
}

/// One request to the worker. Each carries its own reply channel, so a caller
/// that gives up simply drops the receiver and the worker discovers the
/// abandonment when its send fails.
pub(crate) enum Request {
    Command {
        cmd: String,
        /// `None` means "the port's own defaults" — see [`ResponseBudget`].
        budget: Option<ResponseBudget>,
        reply: mpsc::Sender<BridgeResult<AtResponse>>,
    },
    /// `Ok(None)` means the deadline passed with nothing buffered and nothing in
    /// flight — the normal state of an idle URC poll, and explicitly *not* a
    /// fault. See [`Session::read_line_raw`].
    ReadLine {
        reply: mpsc::Sender<BridgeResult<Option<String>>>,
    },
    /// Bring a suspect channel back into step ([`Session::sync`]).
    Resync {
        reply: mpsc::Sender<BridgeResult<()>>,
    },
    /// Prove a freshly opened port is in step before its first command
    /// ([`Session::sync`]). Sent once, by `AtCommander::start_worker`.
    Sync {
        reply: mpsc::Sender<BridgeResult<()>>,
    },
}

/// A port plus the buffer that survives across commands.
pub(crate) struct Session {
    port: Box<dyn ReadWrite + Send>,
    /// Bytes read but not yet consumed as a complete line.
    rx: Vec<u8>,
    /// Set when a command gave up waiting, or on any read error. The abandoned
    /// reply may still be in flight, so the next command syncs before it
    /// writes — otherwise that reply is read as the next command's answer and
    /// every command after it is off by one, silently and permanently.
    /// Cleared only by a successful [`Session::sync`].
    desynced: bool,
    /// Per-read timeout configured on the underlying port. Used as the overall
    /// deadline for one command, so a response that dribbles forever without
    /// terminating still fails.
    timeout: Duration,
    /// Which port this session drives, for logs and error detail: the device
    /// path, or a fixed name for a test stream.
    label: String,
}

impl Session {
    pub(crate) fn new(
        port: Box<dyn ReadWrite + Send>,
        timeout: Duration,
        label: impl Into<String>,
    ) -> Self {
        Self {
            port,
            rx: Vec::new(),
            desynced: false,
            timeout,
            label: label.into(),
        }
    }

    /// Which port this session drives.
    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    /// Pull one complete line out of `rx`, if there is one.
    fn take_line(&mut self) -> Option<String> {
        self.take_line_counted().map(|(line, _)| line)
    }

    /// Like [`Session::take_line`], also returning how many raw bytes the
    /// line took, line ending included.
    fn take_line_counted(&mut self) -> Option<(String, usize)> {
        let idx = self.rx.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = self.rx.drain(..=idx).collect();
        Some((String::from_utf8_lossy(&line).trim().to_string(), idx + 1))
    }

    /// Read more bytes into `rx`. `Ok(false)` means the read timed out with
    /// nothing new.
    ///
    /// Every error path here leaves `desynced` set, because every one of them
    /// can return with unparsed bytes still sitting in `rx`. Without that, the
    /// next `send_command` skips its sync and parses the leftovers as its own
    /// reply — the permanent off-by-one this whole module exists to prevent,
    /// reintroduced through the error path instead of the timeout path.
    fn fill(&mut self) -> BridgeResult<bool> {
        let mut buf = [0u8; 1024];
        match self.port.read(&mut buf) {
            // End of file, not "nothing to read": the modem has gone (unplugged,
            // USB re-enumerated, peer closed). Reporting this as a timeout made
            // the read loops spin without ever blocking — a 100%-CPU busy loop
            // for the whole deadline, on every command, for as long as the port
            // stayed gone.
            Ok(0) => {
                self.desynced = true;
                Err(BridgeError::Discovery(
                    "the modem port reported end-of-file; the device has gone away".into(),
                ))
            }
            Ok(n) => {
                if self.rx.len() + n > MAX_BUFFERED_BYTES {
                    self.desynced = true;
                    return Err(BridgeError::Discovery(format!(
                        "AT response exceeded {MAX_BUFFERED_BYTES} buffered bytes without \
                         terminating; treating the channel as desynchronised"
                    )));
                }
                self.rx.extend_from_slice(&buf[..n]);
                Ok(true)
            }
            // Both spellings of "nothing to read right now": `serialport`
            // surfaces an expired poll as `TimedOut`, while a socket with
            // `SO_RCVTIMEO` reports `WouldBlock` (EAGAIN). Treating only one of
            // them as a timeout would turn the other into a hard error and
            // tear down a perfectly healthy channel.
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                Ok(false)
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(e) => {
                self.desynced = true;
                Err(BridgeError::Discovery(format!("AT read error: {e}")))
            }
        }
    }

    /// The bounds an ordinary command runs under: this port's configured read
    /// timeout as the overall deadline, and the default line cap.
    fn default_budget(&self) -> ResponseBudget {
        ResponseBudget {
            max_lines: MAX_RESPONSE_LINES,
            timeout: self.timeout,
        }
    }

    /// Write a command and collect its response under this port's default
    /// bounds. See [`Session::send_command_within`] for a command that needs
    /// its own.
    pub(crate) fn send_command(&mut self, cmd: &str) -> BridgeResult<AtResponse> {
        self.send_command_within(cmd, self.default_budget())
    }

    /// Write a command and collect its response, bounded by an overall
    /// deadline rather than only a per-read one.
    pub(crate) fn send_command_within(
        &mut self,
        cmd: &str,
        budget: ResponseBudget,
    ) -> BridgeResult<AtResponse> {
        // Only after a give-up: anything in flight then is the abandoned
        // command's answer, and mistaking it for this command's is what turns
        // one timeout into a permanently off-by-one channel. A drain is not
        // enough, because the answer may still be on its way (#237). On the
        // healthy path the channel is already in step and a sync would cost
        // two round trips on every command.
        if self.desynced {
            self.sync(self.timeout)?;
        }

        let full = format!("{cmd}\r\n");
        // A failed write may still have put a partial command on the wire, so
        // the modem's next reply is to something we cannot name. That is a
        // desync by definition.
        if let Err(e) = self.port.write_all(full.as_bytes()) {
            self.desynced = true;
            return Err(BridgeError::Discovery(format!("AT write failed: {e}")));
        }
        // Deliberately no `tcdrain`-style flush here: on a 115200 baud line an
        // AT command is microseconds of wire time, so there is nothing to wait
        // for, and `flush()` on a serial port is itself unbounded — if the line
        // *is* wedged, blocking until the kernel's output queue drains is
        // precisely the wrong thing to do.
        tracing::trace!(target: "at", cmd = cmd, "sent");

        let deadline = Instant::now() + budget.timeout;
        let mut lines = Vec::new();
        loop {
            while let Some(line) = self.take_line() {
                if line.is_empty() {
                    continue;
                }
                tracing::trace!(target: "at", line = %line, "recv");
                if line == "OK" {
                    return Ok(AtResponse::Ok(lines));
                } else if line == "ERROR" {
                    return Ok(AtResponse::Error("ERROR".into()));
                } else if let Some(cme) = line.strip_prefix("+CME ERROR: ") {
                    let code = cme.parse::<u32>().unwrap_or(0);
                    return Ok(AtResponse::CmeError(code, cme.into()));
                }
                if lines.len() >= budget.max_lines {
                    // Abandoning a response mid-stream leaves the rest of it
                    // arriving behind us, so the channel must be treated as
                    // desynchronised exactly as a timeout is.
                    self.desynced = true;
                    return Err(BridgeError::Discovery(format!(
                        "AT response exceeded {} lines without terminating",
                        budget.max_lines
                    )));
                }
                lines.push(line);
            }
            if Instant::now() >= deadline {
                self.desynced = true;
                // Same error text as before the rewrite: callers and
                // `sim_recovery`'s greps already key off it.
                return Err(BridgeError::Discovery("AT command timeout".into()));
            }
            self.fill()?;
        }
    }

    /// Read a single line, bounded by the same overall deadline.
    ///
    /// `Ok(None)` is "the deadline passed and there was nothing to read", which
    /// is the *normal* outcome for the circuit-switched worker's URC poll: it
    /// calls this in a tight loop waiting for a `RING`/`+CLIP:`/`+CMTI:` that
    /// mostly is not there. Reporting that as an error made the channel
    /// permanently `Suspect`, so every subsequent command resynchronised first —
    /// and a resync *discards buffered bytes*, throwing away exactly the URCs
    /// this loop exists to catch, as well as roughly doubling the cost of every
    /// idle iteration.
    ///
    /// A deadline reached with a *partial* line still in `rx` is a different
    /// thing entirely: a reply is genuinely in flight and we are abandoning it
    /// halfway, so that does set `desynced` and does return an error.
    pub(crate) fn read_line_raw(&mut self) -> BridgeResult<Option<String>> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(line) = self.take_line() {
                return Ok(Some(line));
            }
            if Instant::now() >= deadline {
                if self.rx.is_empty() {
                    // Nothing buffered, nothing in flight: an idle line.
                    return Ok(None);
                }
                self.desynced = true;
                return Err(BridgeError::Discovery(
                    "AT read timeout with a partial line buffered".into(),
                ));
            }
            self.fill()?;
        }
    }

    /// Bring the channel into step: afterwards, the next line read answers
    /// the next command written (research D2). Returns how many stale bytes
    /// it discarded on the way.
    ///
    /// It writes [`SYNC_FIRST`] and reads, discarding every line, until a
    /// `+CMEE:` line followed by `OK`; then writes [`SYNC_SECOND`] and does
    /// the same for `+CSCS:`. Replies come out in command order, so whatever
    /// an abandoned command (ours, or another holder's) was still producing
    /// comes out first and is discarded. A stale `OK` cannot pass for either
    /// reply. A stale reply from one earlier abandoned sync can pass for our
    /// first, but then our own first reply is discarded while we wait for the
    /// second, and the session still ends in step.
    ///
    /// The echo of a query and blank lines around our own replies are not
    /// counted as stale. `ERROR` cannot be told apart from a stale reply, so
    /// a modem that refuses both queries fails the sync.
    ///
    /// The whole exchange runs against one deadline, `now + bound`. On the
    /// deadline it leaves `desynced` set and returns the same
    /// `AT command timeout` every classifier already reads as a timeout.
    pub(crate) fn sync(&mut self, bound: Duration) -> BridgeResult<usize> {
        let deadline = Instant::now() + bound;
        let mut discarded = self.rx.len();
        self.rx.clear();
        for (query, prefix) in [(SYNC_FIRST, "+CMEE:"), (SYNC_SECOND, "+CSCS:")] {
            if let Err(e) = self.port.write_all(format!("{query}\r\n").as_bytes()) {
                self.desynced = true;
                return Err(BridgeError::Discovery(format!("AT write failed: {e}")));
            }
            tracing::trace!(target: "at", cmd = query, "sent");
            discarded += self.read_sync_reply(query, prefix, deadline)?;
        }
        self.desynced = false;
        tracing::debug!(port = %self.label, discarded_bytes = discarded, "AT channel in step");
        Ok(discarded)
    }

    /// Read until a line starting `prefix` followed by `OK`, the reply to
    /// `query`. Returns the stale bytes discarded on the way.
    fn read_sync_reply(
        &mut self,
        query: &str,
        prefix: &str,
        deadline: Instant,
    ) -> BridgeResult<usize> {
        let mut discarded = 0;
        // Blank lines not yet attributed: part of our reply if our reply
        // comes next, stale otherwise.
        let mut blank = 0;
        // Bytes of a `prefix` line read so far, still waiting for its `OK`.
        let mut candidate: Option<usize> = None;
        loop {
            while let Some((line, len)) = self.take_line_counted() {
                if line.is_empty() {
                    blank += len;
                    continue;
                }
                tracing::trace!(target: "at", line = %line, "recv");
                if line.starts_with(prefix) {
                    // Two in a row: the earlier one was stale.
                    discarded += candidate.replace(blank + len).unwrap_or(0);
                } else if line == "OK" && candidate.is_some() {
                    return Ok(discarded);
                } else if line == query {
                    // The modem echoing our own query.
                    discarded += blank;
                } else {
                    // A final result cannot follow our `prefix` line, so
                    // that line belonged to the stale reply it ends.
                    if line == "ERROR" || line.starts_with("+CME ERROR") {
                        discarded += candidate.take().unwrap_or(0);
                    }
                    discarded += blank + len;
                }
                blank = 0;
            }
            if Instant::now() >= deadline {
                self.desynced = true;
                return Err(BridgeError::Discovery("AT command timeout".into()));
            }
            self.fill()?;
        }
    }
}

/// Run the worker until its request channel closes.
pub(crate) fn run(mut session: Session, rx: mpsc::Receiver<Request>) {
    while let Ok(req) = rx.recv() {
        match req {
            Request::Command { cmd, budget, reply } => {
                let result = match budget {
                    Some(budget) => session.send_command_within(&cmd, budget),
                    None => session.send_command(&cmd),
                };
                // A failed send means the caller timed out and went away. The
                // reply is now stale, and the *next* command must not see it:
                // the caller marks the channel suspect, and the sync it runs
                // first discards the reply.
                if reply.send(result).is_err() {
                    tracing::debug!(
                        cmd = %cmd,
                        "the caller abandoned this AT command before it completed"
                    );
                }
            }
            Request::ReadLine { reply } => {
                let _ = reply.send(session.read_line_raw());
            }
            Request::Resync { reply } | Request::Sync { reply } => {
                let _ = reply.send(session.sync(session.timeout).map(|_| ()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// A real connected socket pair standing in for the serial port.
    ///
    /// Constitution I: these are genuine OS file descriptors with genuine
    /// blocking and timeout semantics, not an in-memory script — a read with
    /// nothing to read really does block until the timeout really does expire,
    /// which is the exact behaviour under test.
    fn port(timeout: Duration) -> (Session, UnixStream) {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        ours.set_read_timeout(Some(timeout)).expect("read timeout");
        (Session::new(Box::new(ours), timeout, "test"), theirs)
    }

    /// Drive the far end as a modem would: wait for a command, wait `delay`,
    /// then answer. One entry per expected command.
    ///
    /// The delay is what makes "answered too late" reproducible — that is the
    /// case that used to desynchronise the channel permanently, and it cannot
    /// be expressed by a script that simply has bytes waiting up front.
    fn spawn_modem(
        mut sock: UnixStream,
        script: Vec<(Duration, &'static str)>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            for (delay, reply) in script {
                match sock.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                std::thread::sleep(delay);
                if !reply.is_empty() && sock.write_all(reply.as_bytes()).is_err() {
                    return;
                }
            }
            // Hold the socket open so the far end sees silence, not EOF.
            std::thread::sleep(Duration::from_secs(3));
        })
    }

    #[test]
    fn a_modem_that_never_replies_times_out_rather_than_hanging() {
        // The production fault: the port accepts the command and never answers.
        // Pre-fix this parked the calling thread in read(2) indefinitely.
        let (mut s, _modem) = port(Duration::from_millis(100));
        let started = Instant::now();
        let err = s.send_command("AT+CSIM=10,\"00A40004\"").unwrap_err();
        assert!(err.to_string().contains("AT command timeout"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must fail on its deadline, not hang"
        );
    }

    #[test]
    fn a_late_reply_after_a_timeout_is_not_read_as_the_next_answer() {
        // #237. The first command times out and its reply arrives only after
        // the next command has been written: a slow SIM still working on an
        // abandoned APDU. A drain before the write finds nothing and lets
        // the stale reply through as the next command's answer, and every
        // command after that is off by one. The sync proves the channel is
        // in step before the next command is written.
        let (mut s, modem) = port(Duration::from_millis(100));
        let script = test_modem::Script {
            late_after: Some((
                "AT+CSIM".into(),
                Duration::from_millis(50),
                "\r\n+CSIM: 4,\"9000\"\r\n\r\nOK\r\n".into(),
            )),
            ..Default::default()
        }
        .reply(
            "AT+CIMI",
            Duration::ZERO,
            "\r\n001010000000001\r\n\r\nOK\r\n",
        );
        let _events = test_modem::spawn(modem, script);

        assert!(
            s.send_command("AT+CSIM=10,\"00A40004\"").is_err(),
            "the first command must give up rather than wait indefinitely"
        );
        match s.send_command("AT+CIMI").expect("second command") {
            AtResponse::Ok(lines) => assert_eq!(
                lines,
                vec!["001010000000001".to_string()],
                "the IMSI query must return its own answer, not the abandoned CSIM reply"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn a_normal_command_still_returns_its_lines() {
        let (mut s, modem) = port(Duration::from_millis(500));
        let _m = spawn_modem(modem, vec![(Duration::ZERO, "+CSQ: 21,99\r\nOK\r\n")]);
        match s.send_command("AT+CSQ").unwrap() {
            AtResponse::Ok(lines) => assert_eq!(lines, vec!["+CSQ: 21,99".to_string()]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn cme_errors_are_still_surfaced_verbatim() {
        // `+CME ERROR: 14` is "SIM busy" — the symptom in the incident's
        // sim-reset log, and callers match on this shape.
        let (mut s, modem) = port(Duration::from_millis(500));
        let _m = spawn_modem(modem, vec![(Duration::ZERO, "+CME ERROR: 14\r\n")]);
        match s.send_command("AT+CSIM=10,\"00A40004\"").unwrap() {
            AtResponse::CmeError(code, _) => assert_eq!(code, 14),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_unterminated_flood_is_bounded_rather_than_growing_without_limit() {
        // A modem emitting unsolicited output that never terminates must fail,
        // not spin or grow without limit (FR-006).
        let (mut s, mut modem) = port(Duration::from_secs(2));
        let flood: String = (0..MAX_RESPONSE_LINES + 50)
            .map(|i| format!("+JUNK: {i}\r\n"))
            .collect();
        modem.write_all(flood.as_bytes()).expect("flood");
        let err = s.send_command("AT").unwrap_err();
        assert!(err.to_string().contains("without terminating"), "{err}");
    }

    #[test]
    fn a_bulk_listing_larger_than_the_default_cap_still_completes() {
        // The 2026-08-22 regression, in miniature. `AT+CMGL="ALL"` over a full
        // modem store legitimately returns hundreds of lines; under the default
        // cap it aborted at 256 every time, and because the SMS sweep can only
        // delete what it first managed to list, storage could never drain.
        let (mut s, mut modem) = port(Duration::from_secs(2));
        let messages = 255;
        let mut listing = String::new();
        for i in 0..messages {
            listing.push_str(&format!(
                "+CMGL: {i},\"REC READ\",\"+919000000000\",,\"26/08/22,09:07:48+22\"\r\n"
            ));
            listing.push_str("body line one\r\nbody line two\r\n");
        }
        listing.push_str("OK\r\n");
        assert!(
            listing.lines().count() > MAX_RESPONSE_LINES,
            "the fixture must actually exceed the default cap"
        );
        modem.write_all(listing.as_bytes()).expect("listing");

        let budget = crate::sms::reader::BULK_LIST_BUDGET;
        match s
            .send_command_within("AT+CMGL=\"ALL\"", budget)
            .expect("a bulk listing within its own budget must succeed")
        {
            AtResponse::Ok(lines) => assert_eq!(
                lines.len(),
                messages * 3,
                "every header and body line must survive"
            ),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(
            !s.desynced,
            "a listing that terminated normally must leave the channel healthy"
        );
    }

    #[test]
    fn a_bulk_budget_still_bounds_a_flood_that_never_terminates() {
        // Raising the cap for listings must not remove the bound: a modem that
        // never sends `OK` still has to fail rather than grow without limit
        // (FR-006). Deliberately checks the *line* bound, with a budget whose
        // deadline is generous enough that it cannot be what fires.
        let (mut s, mut modem) = port(Duration::from_millis(200));
        let budget = ResponseBudget {
            max_lines: 300,
            timeout: Duration::from_secs(10),
        };
        let flood: String = (0..budget.max_lines + 50)
            .map(|i| format!("+JUNK: {i}\r\n"))
            .collect();
        modem.write_all(flood.as_bytes()).expect("flood");
        let err = s
            .send_command_within("AT+CMGL=\"ALL\"", budget)
            .unwrap_err();
        assert!(err.to_string().contains("300 lines"), "{err}");
        assert!(s.desynced);
    }

    #[test]
    fn a_bulk_budget_does_not_relax_the_default_for_other_commands() {
        // The budget is per-command, not a mode the channel enters: an ordinary
        // command issued after a listing must still be caught at 256.
        let (mut s, mut modem) = port(Duration::from_secs(2));
        let flood: String = (0..MAX_RESPONSE_LINES + 50)
            .map(|i| format!("+JUNK: {i}\r\n"))
            .collect();
        modem.write_all(flood.as_bytes()).expect("flood");
        let err = s.send_command("AT+CSQ").unwrap_err();
        assert!(err.to_string().contains("256 lines"), "{err}");
    }

    #[test]
    fn an_unterminated_flood_leaves_the_channel_desynced() {
        // Bailing out mid-response abandons the rest of it, still arriving. If
        // that does not set `desynced`, the next command skips its sync and
        // parses the leftovers as its own answer — the same permanent off-by-one
        // the timeout path is careful to avoid, arrived at through the error path.
        let (mut s, mut modem) = port(Duration::from_millis(300));
        let flood: String = (0..MAX_RESPONSE_LINES + 50)
            .map(|i| format!("+JUNK: {i}\r\n"))
            .collect();
        modem.write_all(flood.as_bytes()).expect("flood");
        assert!(s.send_command("AT").is_err());
        assert!(
            s.desynced,
            "a response abandoned mid-stream must mark the channel desynchronised"
        );
    }

    #[test]
    fn a_command_after_an_abandoned_flood_reads_its_own_answer() {
        // The consequence, stated end to end.
        let (mut s, mut modem) = port(Duration::from_millis(300));
        let flood: String = (0..MAX_RESPONSE_LINES + 50)
            .map(|i| format!("+JUNK: {i}\r\n"))
            .collect();
        modem.write_all(flood.as_bytes()).expect("flood");
        assert!(s.send_command("AT").is_err());
        assert!(s.desynced);

        // The sync before the next command reads past the rest of the flood;
        // the far end answers the sync and then `AT+CSQ`. The `AT` above is
        // still unread at the far end and gets no answer.
        let script = test_modem::Script::default().reply(
            "AT+CSQ",
            Duration::ZERO,
            "\r\n+CSQ: 19,99\r\n\r\nOK\r\n",
        );
        let _events = test_modem::spawn(modem, script);
        match s.send_command("AT+CSQ").expect("second command") {
            AtResponse::Ok(lines) => assert_eq!(
                lines,
                vec!["+CSQ: 19,99".to_string()],
                "the second command must not be handed the abandoned flood"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_idle_read_with_nothing_pending_is_not_a_fault() {
        // The circuit-switched worker polls this in a loop waiting for a
        // `RING`/`+CLIP:`/`+CMTI:` that is usually not there. Reporting that as
        // an error marked the channel `Suspect`, and the resync that followed
        // discarded buffered bytes — dropping the very URCs the poll exists to
        // catch. An empty idle read must be an ordinary, stateless "nothing yet".
        let (mut s, _modem) = port(Duration::from_millis(100));
        assert_eq!(s.read_line_raw().expect("idle read"), None);
        assert!(
            !s.desynced,
            "an idle poll with nothing in flight is not a desynchronisation"
        );
        // Repeatable: still no state accumulating across polls.
        assert_eq!(s.read_line_raw().expect("idle read"), None);
        assert!(!s.desynced);
    }

    #[test]
    fn an_idle_poll_does_not_swallow_the_urc_that_follows_it() {
        // What the bug actually cost on a GSM line: a missed inbound call.
        let (mut s, mut modem) = port(Duration::from_millis(100));
        assert_eq!(s.read_line_raw().expect("idle read"), None);
        modem.write_all(b"RING\r\n").expect("urc");
        assert_eq!(
            s.read_line_raw().expect("urc read"),
            Some("RING".to_string()),
            "a URC arriving after an idle poll must still be delivered"
        );
    }

    #[test]
    fn a_partial_line_at_the_deadline_is_a_desync() {
        // The other half of the distinction: bytes *are* in flight and we are
        // abandoning them halfway, so the channel genuinely is out of step.
        let (mut s, mut modem) = port(Duration::from_millis(100));
        modem.write_all(b"+CLIP: \"+919000").expect("partial");
        std::thread::sleep(Duration::from_millis(50));
        let err = s
            .read_line_raw()
            .expect_err("a partial line must not be Ok");
        assert!(err.to_string().contains("partial line"), "{err}");
        assert!(s.desynced);
    }

    #[test]
    fn a_closed_port_reports_eof_rather_than_spinning() {
        // `Ok(0)` is EOF, not "nothing to read". Mapping it to a timeout made
        // both read loops spin with no blocking call in them at all — 100% CPU
        // for the whole deadline, per read, for as long as the modem stayed
        // unplugged. Asserted through `read_line_raw` because it is a pure read
        // path: `send_command` writes first, and on a socket a dropped peer
        // surfaces there as `EPIPE` before any read happens.
        let (mut s, modem) = port(Duration::from_secs(5));
        drop(modem);
        let started = Instant::now();
        let err = s.read_line_raw().expect_err("EOF must be an error");
        assert!(err.to_string().contains("end-of-file"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "EOF must fail immediately, not spin until the deadline (took {:?})",
            started.elapsed()
        );
        assert!(s.desynced, "a vanished port leaves nothing trustworthy");
    }

    #[test]
    fn a_failed_write_marks_the_channel_desynced() {
        // A write that fails may still have put a partial command on the wire,
        // so the modem's next utterance answers something we cannot name.
        let (mut s, modem) = port(Duration::from_millis(100));
        drop(modem);
        let err = s
            .send_command("AT+CSQ")
            .expect_err("write to a closed peer");
        assert!(err.to_string().contains("AT write failed"), "{err}");
        assert!(s.desynced);
    }

    #[test]
    fn resync_succeeds_when_the_modem_answers_again() {
        // The cheap remedy: the worker was merely slow, so one sync rescues
        // the channel with no restart and no lost line.
        let (mut s, modem) = port(Duration::from_millis(200));
        let script = test_modem::Script::default().reply(
            "AT+CIMI",
            Duration::from_millis(400),
            "\r\n001010000000001\r\n\r\nOK\r\n",
        );
        let _events = test_modem::spawn(modem, script);
        let err = s.send_command("AT+CIMI").expect_err("provoke a timeout");
        assert!(err.to_string().contains("AT command timeout"), "{err}");
        s.sync(s.timeout).expect("resync");
        assert!(!s.desynced, "a successful sync clears the desync");
    }

    #[test]
    fn resync_fails_on_a_silent_modem() {
        let (mut s, _modem) = port(Duration::from_millis(100));
        let err = s
            .sync(s.timeout)
            .expect_err("a silent modem cannot be in step");
        assert!(err.to_string().contains("AT command timeout"), "{err}");
    }

    #[test]
    fn a_command_after_a_successful_resync_reads_its_own_answer() {
        // The late IMSI reply lands during the sync and is discarded there;
        // the next command reads its own answer.
        let (mut s, modem) = port(Duration::from_millis(200));
        let script = test_modem::Script::default()
            .reply(
                "AT+CIMI",
                Duration::from_millis(400),
                "\r\n001010000000001\r\n\r\nOK\r\n",
            )
            .reply("AT+CSQ", Duration::ZERO, "\r\n+CSQ: 30,99\r\n\r\nOK\r\n");
        let _events = test_modem::spawn(modem, script);
        assert!(s.send_command("AT+CIMI").is_err(), "provoke a timeout");
        s.sync(s.timeout).expect("resync");
        match s.send_command("AT+CSQ").unwrap() {
            AtResponse::Ok(lines) => assert_eq!(lines, vec!["+CSQ: 30,99".to_string()]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ---- #237: the sync (research D2) ----

    const STALE_CSIM: &str = "\r\n+CSIM: 4,\"FFFF\"\r\n\r\nOK\r\n";
    const CSQ_REPLY: &str = "\r\n+CSQ: 21,99\r\n\r\nOK\r\n";

    /// The next command reads its own scripted `+CSQ`, i.e. the channel is
    /// in step.
    fn assert_in_step(s: &mut Session) {
        match s
            .send_command("AT+CSQ")
            .expect("the command after the sync")
        {
            AtResponse::Ok(lines) => assert_eq!(
                lines,
                vec!["+CSQ: 21,99".to_string()],
                "the command after the sync must read its own reply"
            ),
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn csq_modem(modem: UnixStream, script: test_modem::Script) -> test_modem::Events {
        test_modem::spawn(modem, script.reply("AT+CSQ", Duration::ZERO, CSQ_REPLY))
    }

    #[test]
    fn sync_discards_a_stale_reply_sent_before_it() {
        let (mut s, mut modem) = port(Duration::from_millis(500));
        modem.write_all(STALE_CSIM.as_bytes()).expect("stale");
        let _events = csq_modem(modem, test_modem::Script::default());
        let discarded = s.sync(Duration::from_secs(2)).expect("sync");
        assert_eq!(discarded, STALE_CSIM.len(), "every stale byte is counted");
        assert_in_step(&mut s);
    }

    #[test]
    fn sync_is_not_fooled_by_a_lone_stale_ok() {
        // A bare `AT` probe takes a stale `OK` as its own answer; the sync
        // waits for a reply only its own query produces.
        let (mut s, mut modem) = port(Duration::from_millis(500));
        modem.write_all(b"\r\nOK\r\n").expect("stale");
        let _events = csq_modem(modem, test_modem::Script::default());
        assert_eq!(s.sync(Duration::from_secs(2)).expect("sync"), 6);
        assert_in_step(&mut s);
    }

    #[test]
    fn sync_waits_out_a_command_the_modem_is_still_running() {
        // F3: the abandoned reply arrives only after the sync's first write.
        // A drain would have found nothing; the sync reads past it.
        let (mut s, modem) = port(Duration::from_millis(100));
        let script = test_modem::Script {
            before_first_sync: Some((Duration::from_millis(300), STALE_CSIM.into())),
            ..Default::default()
        };
        let _events = csq_modem(modem, script);
        let discarded = s.sync(Duration::from_secs(2)).expect("sync");
        assert_eq!(discarded, STALE_CSIM.len());
        assert_in_step(&mut s);
    }

    #[test]
    fn sync_survives_a_stale_cmee_reply_from_an_earlier_sync() {
        // A sync abandoned after its first query leaves a `+CMEE:` reply. We
        // take it as ours, then discard our own `+CMEE:` while waiting for
        // `+CSCS:`, and end in step.
        let (mut s, mut modem) = port(Duration::from_millis(500));
        modem
            .write_all(test_modem::CMEE_REPLY.as_bytes())
            .expect("stale");
        let _events = csq_modem(modem, test_modem::Script::default());
        s.sync(Duration::from_secs(2)).expect("sync");
        assert_in_step(&mut s);
    }

    #[test]
    fn sync_survives_a_stale_cscs_reply_from_an_earlier_sync() {
        let (mut s, mut modem) = port(Duration::from_millis(500));
        modem
            .write_all(test_modem::CSCS_REPLY.as_bytes())
            .expect("stale");
        let _events = csq_modem(modem, test_modem::Script::default());
        let discarded = s.sync(Duration::from_secs(2)).expect("sync");
        assert_eq!(discarded, test_modem::CSCS_REPLY.len());
        assert_in_step(&mut s);
    }

    #[test]
    fn sync_ignores_command_echo() {
        // Echo is the power-on default and only the CS pool sends `ATE0`, so
        // the echo of the sync's own query is neither its reply nor stale.
        let (mut s, modem) = port(Duration::from_millis(500));
        let script = test_modem::Script {
            echo: true,
            ..Default::default()
        };
        let _events = test_modem::spawn(modem, script);
        assert_eq!(s.sync(Duration::from_secs(2)).expect("sync"), 0);
    }

    #[test]
    fn sync_with_nothing_stale_reports_zero_and_sends_exactly_two_queries() {
        let (mut s, modem) = port(Duration::from_millis(500));
        let events = test_modem::spawn(modem, test_modem::Script::default());
        assert_eq!(s.sync(Duration::from_secs(2)).expect("sync"), 0);
        // Let the far end log anything else we might have sent.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            test_modem::received(&events),
            vec!["AT+CMEE?".to_string(), "AT+CSCS?".to_string()],
            "two read-only queries and nothing else (FR-004)"
        );
    }

    #[test]
    fn sync_fails_within_its_bound_on_a_silent_port() {
        let read_timeout = Duration::from_millis(100);
        let bound = Duration::from_millis(300);
        let (mut s, _modem) = port(read_timeout);
        let started = Instant::now();
        let err = s.sync(bound).expect_err("a silent port cannot be in step");
        let took = started.elapsed();
        assert!(err.to_string().contains("AT command timeout"), "{err}");
        assert!(took >= bound, "gave up early: {took:?}");
        assert!(
            took < bound + read_timeout + Duration::from_millis(50),
            "{took:?}"
        );
        assert!(s.desynced, "a failed sync leaves the channel desynced");
    }

    #[test]
    fn sync_fails_when_the_modem_refuses_both_queries() {
        let (mut s, modem) = port(Duration::from_millis(100));
        let script = test_modem::Script {
            refuse_sync: true,
            ..Default::default()
        };
        let _events = test_modem::spawn(modem, script);
        let bound = Duration::from_millis(400);
        let started = Instant::now();
        let err = s.sync(bound).expect_err("ERROR is not the sync's reply");
        assert!(err.to_string().contains("AT command timeout"), "{err}");
        assert!(started.elapsed() < bound + Duration::from_millis(200));
    }

    #[test]
    fn sync_sends_the_second_query_only_after_the_first_reply() {
        // The ordering is the proof: a reply to the second query can only
        // follow our own first reply if the second was written after it.
        let (mut s, modem) = port(Duration::from_millis(500));
        let script = test_modem::Script {
            sync_delay: Duration::from_millis(100),
            ..Default::default()
        };
        let events = test_modem::spawn(modem, script);
        s.sync(Duration::from_secs(2)).expect("sync");
        let events = events.lock().unwrap().clone();
        let cmee_tx = events.iter().position(|e| e == "tx:AT+CMEE?");
        let cscs_rx = events.iter().position(|e| e == "rx:AT+CSCS?");
        assert!(
            matches!((cmee_tx, cscs_rx), (Some(tx), Some(rx)) if tx < rx),
            "AT+CSCS? must arrive after the +CMEE reply was written: {events:?}"
        );
    }
}

/// A scripted modem on the far end of a real `UnixStream`, for every test that
/// drives a port through a sync (this module, `at_commander`, `usim`,
/// `vowifi::usim_bridge`).
///
/// It reads command lines and answers each one the way a modem does: the two
/// sync queries with their standard replies, other commands from a script,
/// anything unscripted with silence. Real fds, real blocking, real timeouts
/// (constitution I), so a late or missing answer behaves exactly as it does on
/// a tty.
#[cfg(test)]
pub(crate) mod test_modem {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    pub(crate) const CMEE_REPLY: &str = "\r\n+CMEE: 1\r\n\r\nOK\r\n";
    pub(crate) const CSCS_REPLY: &str = "\r\n+CSCS: \"GSM\"\r\n\r\nOK\r\n";

    /// How the far end behaves.
    #[derive(Default)]
    pub(crate) struct Script {
        /// Echo every command line back before answering it.
        pub(crate) echo: bool,
        /// Answer both sync queries `ERROR`.
        pub(crate) refuse_sync: bool,
        /// Wait this long before answering a sync query.
        pub(crate) sync_delay: Duration,
        /// On the first `AT+CMEE?` only: wait, then send these bytes before
        /// answering it. This is a reply the modem was still producing for
        /// an earlier, abandoned command.
        pub(crate) before_first_sync: Option<(Duration, String)>,
        /// A command matching the pattern gets no answer of its own; its
        /// reply (the bytes) is sent after the delay once the *next* command
        /// has arrived, ahead of that command's answer. This is a reply the
        /// near end gave up on, arriving after its next write.
        pub(crate) late_after: Option<(String, Duration, String)>,
        /// Other commands: the first entry whose pattern the command line
        /// contains is answered with its reply after its delay. An empty
        /// reply is silence.
        pub(crate) replies: Vec<(String, Duration, String)>,
    }

    impl Script {
        pub(crate) fn reply(mut self, pattern: &str, delay: Duration, reply: &str) -> Self {
            self.replies
                .push((pattern.to_string(), delay, reply.to_string()));
            self
        }
    }

    /// What the far end saw and did, in order: `rx:<command line>` when a
    /// command arrived and `tx:<command line>` when its answer was written.
    pub(crate) type Events = Arc<Mutex<Vec<String>>>;

    /// Run `script` on `sock` until the near end closes.
    pub(crate) fn spawn(sock: UnixStream, script: Script) -> Events {
        let events: Events = Arc::default();
        let log = Arc::clone(&events);
        std::thread::spawn(move || serve(sock, script, &log));
        events
    }

    fn serve(mut sock: UnixStream, mut script: Script, log: &Events) {
        let mut pending = Vec::new();
        let mut queue = VecDeque::new();
        let mut armed: Option<(Duration, String)> = None;
        loop {
            let Some(cmd) = next_command(&mut sock, &mut pending, &mut queue, log) else {
                return;
            };
            if let Some((wait, late)) = armed.take() {
                std::thread::sleep(wait);
                if sock.write_all(late.as_bytes()).is_err() {
                    return;
                }
            }
            if let Some((pattern, wait, late)) = &script.late_after {
                if cmd.contains(pattern.as_str()) {
                    armed = Some((*wait, late.clone()));
                    script.late_after = None;
                    continue;
                }
            }
            if script.echo && sock.write_all(format!("{cmd}\r\n").as_bytes()).is_err() {
                return;
            }
            let (delay, reply) = match cmd.as_str() {
                "AT+CMEE?" | "AT+CSCS?" if script.refuse_sync => {
                    (script.sync_delay, "\r\nERROR\r\n".to_string())
                }
                "AT+CMEE?" => {
                    if let Some((wait, stale)) = script.before_first_sync.take() {
                        std::thread::sleep(wait);
                        if sock.write_all(stale.as_bytes()).is_err() {
                            return;
                        }
                    }
                    (script.sync_delay, CMEE_REPLY.to_string())
                }
                "AT+CSCS?" => (script.sync_delay, CSCS_REPLY.to_string()),
                _ => script
                    .replies
                    .iter()
                    .find(|(pattern, _, _)| cmd.contains(pattern.as_str()))
                    .map(|(_, delay, reply)| (*delay, reply.clone()))
                    .unwrap_or_default(),
            };
            std::thread::sleep(delay);
            // Whatever the near end wrote while we waited is logged before our
            // answer, so a test can tell "sent after the reply" from "sent
            // while waiting for it".
            drain_available(&mut sock, &mut pending, &mut queue, log);
            if !reply.is_empty() {
                if sock.write_all(reply.as_bytes()).is_err() {
                    return;
                }
                push(log, format!("tx:{cmd}"));
            }
        }
    }

    fn push(log: &Events, event: String) {
        log.lock().unwrap().push(event);
    }

    /// Split complete command lines out of `pending` into `queue`.
    fn split_lines(pending: &mut Vec<u8>, queue: &mut VecDeque<String>, log: &Events) {
        while let Some(idx) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=idx).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if !line.is_empty() {
                push(log, format!("rx:{line}"));
                queue.push_back(line);
            }
        }
    }

    fn next_command(
        sock: &mut UnixStream,
        pending: &mut Vec<u8>,
        queue: &mut VecDeque<String>,
        log: &Events,
    ) -> Option<String> {
        let mut buf = [0u8; 1024];
        loop {
            if let Some(cmd) = queue.pop_front() {
                return Some(cmd);
            }
            match sock.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    split_lines(pending, queue, log);
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return None,
            }
        }
    }

    fn drain_available(
        sock: &mut UnixStream,
        pending: &mut Vec<u8>,
        queue: &mut VecDeque<String>,
        log: &Events,
    ) {
        if sock.set_nonblocking(true).is_err() {
            return;
        }
        let mut buf = [0u8; 1024];
        while let Ok(n) = sock.read(&mut buf) {
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&buf[..n]);
        }
        let _ = sock.set_nonblocking(false);
        split_lines(pending, queue, log);
    }

    /// The command lines the far end received, in order.
    pub(crate) fn received(events: &Events) -> Vec<String> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e.strip_prefix("rx:").map(str::to_string))
            .collect()
    }
}
