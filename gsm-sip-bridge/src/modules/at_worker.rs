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
//! [`Session::resync`] exists to avoid paying that price whenever the worker
//! was merely slow rather than truly stuck.
//!
//! # Why the read buffer lives here
//!
//! It used to be a `BufReader` constructed per command and dropped on return,
//! which silently discarded whatever it had buffered past the terminating
//! `OK`. A reply arriving after a timeout was therefore read as the *next*
//! command's response, and every command after that was off by one —
//! permanently, with no error anywhere. Owning the buffer across commands is
//! what makes a timeout survivable instead of channel-poisoning.

use crate::error::{BridgeError, BridgeResult};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::at_commander::{AtResponse, ReadWrite};

/// Cap on lines accumulated for one response. A modem emitting unsolicited
/// output that never terminates must fail, not grow without limit (FR-006).
const MAX_RESPONSE_LINES: usize = 256;
/// Cap on unparsed bytes held for one response, for the same reason.
const MAX_BUFFERED_BYTES: usize = 64 * 1024;
/// How long a resync round trip may take. Short: it is a bare `AT`, and its
/// whole purpose is to answer "is this channel usable?" quickly.
const RESYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// One request to the worker. Each carries its own reply channel, so a caller
/// that gives up simply drops the receiver and the worker discovers the
/// abandonment when its send fails.
pub(crate) enum Request {
    Command {
        cmd: String,
        reply: mpsc::Sender<BridgeResult<AtResponse>>,
    },
    /// `Ok(None)` means the deadline passed with nothing buffered and nothing in
    /// flight — the normal state of an idle URC poll, and explicitly *not* a
    /// fault. See [`Session::read_line_raw`].
    ReadLine {
        reply: mpsc::Sender<BridgeResult<Option<String>>>,
    },
    /// Drain anything pending and confirm the modem still answers.
    Resync {
        reply: mpsc::Sender<BridgeResult<()>>,
    },
}

/// A port plus the buffer that survives across commands.
pub(crate) struct Session {
    port: Box<dyn ReadWrite + Send>,
    /// Bytes read but not yet consumed as a complete line.
    rx: Vec<u8>,
    /// Set when a command gave up waiting. The abandoned reply may still be in
    /// flight, so the next command drains before it writes — otherwise that
    /// reply is read as the next command's answer and every command after it
    /// is off by one, silently and permanently.
    desynced: bool,
    /// Per-read timeout configured on the underlying port. Used as the overall
    /// deadline for one command, so a response that dribbles forever without
    /// terminating still fails.
    timeout: Duration,
}

impl Session {
    pub(crate) fn new(port: Box<dyn ReadWrite + Send>, timeout: Duration) -> Self {
        Self {
            port,
            rx: Vec::new(),
            desynced: false,
            timeout,
        }
    }

    /// Pull one complete line out of `rx`, if there is one.
    fn take_line(&mut self) -> Option<String> {
        let idx = self.rx.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = self.rx.drain(..=idx).collect();
        Some(String::from_utf8_lossy(&line).trim().to_string())
    }

    /// Read more bytes into `rx`. `Ok(false)` means the read timed out with
    /// nothing new.
    ///
    /// Every error path here leaves `desynced` set, because every one of them
    /// can return with unparsed bytes still sitting in `rx`. Without that, the
    /// next `send_command` skips its drain and parses the leftovers as its own
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

    /// Write a command and collect its response, bounded by an overall
    /// deadline rather than only a per-read one.
    pub(crate) fn send_command(&mut self, cmd: &str) -> BridgeResult<AtResponse> {
        // Only after a give-up: anything in flight then is the abandoned
        // command's answer, and mistaking it for this command's is what turns
        // one timeout into a permanently off-by-one channel. On the healthy
        // path there is nothing to drain and draining anyway would cost a full
        // port timeout on every single command.
        if self.desynced {
            self.discard_pending("the previous command timed out");
            self.desynced = false;
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

        let deadline = Instant::now() + self.timeout;
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
                if lines.len() >= MAX_RESPONSE_LINES {
                    // Abandoning a response mid-stream leaves the rest of it
                    // arriving behind us, so the channel must be treated as
                    // desynchronised exactly as a timeout is.
                    self.desynced = true;
                    return Err(BridgeError::Discovery(format!(
                        "AT response exceeded {MAX_RESPONSE_LINES} lines without terminating"
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

    /// Throw away anything buffered or still arriving from an abandoned reply.
    ///
    /// Bounded by attempts rather than a deadline: each `fill` that finds
    /// nothing already costs one port timeout, so an unbounded sweep would be
    /// far more expensive than the problem it solves. A handful of reads is
    /// enough to clear one abandoned response.
    fn discard_pending(&mut self, why: &str) {
        const MAX_DRAIN_READS: usize = 8;
        let buffered = self.rx.len();
        self.rx.clear();
        let mut drained = 0usize;
        for _ in 0..MAX_DRAIN_READS {
            match self.fill() {
                Ok(true) => {
                    drained += self.rx.len();
                    self.rx.clear();
                }
                _ => break,
            }
        }
        let total = buffered + drained;
        if total > 0 {
            tracing::warn!(
                discarded_bytes = total,
                reason = why,
                "discarded a stale AT reply; the previous command most likely timed out and \
                 its answer arrived late"
            );
        }
    }

    /// Drain and confirm the modem still answers a bare `AT`.
    pub(crate) fn resync(&mut self) -> BridgeResult<()> {
        self.discard_pending("resynchronising the channel");
        self.desynced = false;
        let saved = self.timeout;
        self.timeout = RESYNC_TIMEOUT;
        let result = self.send_command("AT");
        self.timeout = saved;
        match result? {
            AtResponse::Ok(_) => Ok(()),
            other => Err(BridgeError::Discovery(format!(
                "AT resync did not return OK: {other:?}"
            ))),
        }
    }
}

/// Run the worker until its request channel closes.
pub(crate) fn run(mut session: Session, rx: mpsc::Receiver<Request>) {
    while let Ok(req) = rx.recv() {
        match req {
            Request::Command { cmd, reply } => {
                let result = session.send_command(&cmd);
                // A failed send means the caller timed out and went away. The
                // reply is now stale, and the *next* command must not see it —
                // which `discard_pending` guarantees on the way in.
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
            Request::Resync { reply } => {
                let _ = reply.send(session.resync());
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
        (Session::new(Box::new(ours), timeout), theirs)
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
    fn a_late_reply_is_not_returned_to_the_next_command() {
        // The regression that matters most. Before this change `read_response`
        // built a fresh BufReader per command and dropped whatever it had
        // buffered, so a reply arriving after a timeout was handed to the
        // *next* command — and every command after that was off by one,
        // silently and permanently.
        let (mut s, modem) = port(Duration::from_millis(100));
        let _m = spawn_modem(
            modem,
            vec![
                // Answers the first command, but far too late for its caller.
                (Duration::from_millis(300), "+CSIM: 4,\"9000\"\r\nOK\r\n"),
                // Answers the second promptly.
                (Duration::ZERO, "999990000000000\r\nOK\r\n"),
            ],
        );

        assert!(
            s.send_command("AT+CSIM=10,\"00A40004\"").is_err(),
            "the first command must give up rather than wait indefinitely"
        );
        // Let the abandoned answer actually land, so the next command has to
        // cope with it being there.
        std::thread::sleep(Duration::from_millis(400));

        match s.send_command("AT+CIMI").expect("second command") {
            AtResponse::Ok(lines) => assert_eq!(
                lines,
                vec!["999990000000000".to_string()],
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
    fn an_unterminated_flood_leaves_the_channel_desynced() {
        // Bailing out mid-response abandons the rest of it, still arriving. If
        // that does not set `desynced`, the next command skips its drain and
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

        // The modem answers the *next* command, once it arrives — replying
        // before it would be answering a question nobody asked yet, and the
        // pre-write drain would rightly throw it away. The first script entry
        // exists to absorb the `AT` above, which is still sitting unread in the
        // modem's receive queue; without it that command would satisfy the entry
        // meant for `AT+CSQ` and the reply would again arrive too early.
        let _m = spawn_modem(
            modem,
            vec![
                (Duration::ZERO, ""),
                (Duration::ZERO, "+CSQ: 19,99\r\nOK\r\n"),
            ],
        );
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
        // The cheap remedy: the worker was merely slow, so one bare AT round
        // trip rescues the channel with no restart and no lost line.
        let (mut s, modem) = port(Duration::from_millis(200));
        let _m = spawn_modem(
            modem,
            vec![
                (Duration::from_millis(400), ""), // first command: too slow
                (Duration::ZERO, "OK\r\n"),       // the resync's bare AT
            ],
        );
        assert!(s.send_command("AT+CIMI").is_err(), "provoke a timeout");
        assert!(s.resync().is_ok());
    }

    #[test]
    fn resync_fails_on_a_silent_modem() {
        let (mut s, _modem) = port(Duration::from_millis(100));
        assert!(s.resync().is_err());
    }

    #[test]
    fn a_command_after_a_successful_resync_reads_its_own_answer() {
        let (mut s, modem) = port(Duration::from_millis(200));
        let _m = spawn_modem(
            modem,
            vec![
                (Duration::from_millis(400), ""), // first command: too slow
                (Duration::ZERO, "OK\r\n"),       // resync
                (Duration::ZERO, "+CSQ: 30,99\r\nOK\r\n"),
            ],
        );
        assert!(s.send_command("AT+CIMI").is_err(), "provoke a timeout");
        s.resync().expect("resync");
        match s.send_command("AT+CSQ").unwrap() {
            AtResponse::Ok(lines) => assert_eq!(lines, vec!["+CSQ: 30,99".to_string()]),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
