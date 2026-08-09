//! Fake in-memory serial transports shared by this module's tests.
//!
//! Both `probe` and `sim` need an `AtCommander` backed by a script rather than
//! real hardware, and before the split both kept their own copy in the single
//! test module. They live here so there is one copy, not two — the mocks
//! themselves still mirror `at_commander.rs`'s own `MockStream` and
//! `vowifi::usim_bridge`'s `ScriptedModem` deliberately (see those for why
//! each shape exists).

use crate::modules::at_commander::AtCommander;
use std::time::Duration;

/// Single-shot: one `Cursor` of bytes, good for exactly one `send_command`.
struct MockStream {
    reader: std::io::Cursor<Vec<u8>>,
}

impl std::io::Read for MockStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.reader, buf)
    }
}

impl std::io::Write for MockStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn make_commander(response: &str) -> AtCommander {
    AtCommander::from_stream(
        MockStream {
            reader: std::io::Cursor::new(response.as_bytes().to_vec()),
        },
        Duration::from_secs(1),
    )
}

/// A queue of responses, one per `send_command` call — unlike `MockStream`'s
/// single-shot `Cursor`, this survives multiple sequential calls, needed to
/// exercise `recover_and_reprobe_sim`'s AT+CFUN=0/AT+CFUN=1/poll/re-probe
/// sequence. Mirrors `vowifi::usim_bridge`'s own `ScriptedModem` test helper
/// (see its doc comment for why a fresh-`BufReader`-per-call transport needs
/// this instead of a plain `Cursor`).
struct ScriptedModem {
    responses: std::collections::VecDeque<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl std::io::Read for ScriptedModem {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.current.len() {
            let Some(next) = self.responses.pop_front() else {
                return Ok(0);
            };
            self.current = next;
            self.pos = 0;
        }
        let remaining = &self.current[self.pos..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl std::io::Write for ScriptedModem {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn make_scripted_commander(responses: &[&str]) -> AtCommander {
    AtCommander::from_stream(
        ScriptedModem {
            responses: responses.iter().map(|s| s.as_bytes().to_vec()).collect(),
            current: Vec::new(),
            pos: 0,
        },
        Duration::from_secs(1),
    )
}
