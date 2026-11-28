//! What the supervisor keeps of an IMS agent run once it has exited (#239).
//!
//! An agent writes everything to `/tmp/ims-agent-N.out`, and the supervisor
//! empties that file before every start. A run that ended was therefore gone by
//! the time anyone looked, and with it the one line that says why: the stall
//! watchdog's report (activity, phase, last AT command) or the agent's closing
//! `error: …`. On 2026-11-26 a modem line's agent was stopped by the watchdog
//! and then failed three quick starts; each start erased the run before it, and
//! all that survived was the supervisor's `(stalled on the modem)`.
//!
//! So after every exit the supervisor now
//! - copies the run's log to [`previous_run_log`], which the next exit
//!   overwrites;
//! - copies a run the watchdog stopped to [`stalled_run_log`] as well, which
//!   only the next stall overwrites, so quick failures after it do not bury it;
//! - prints the watchdog's report and the agent's last error to its own output
//!   (`journalctl -u gsm-sip-bridge`, `docker logs`), which no restart erases.
//!   Subscriber identifiers in them are masked, as for every agent or tool
//!   output the supervisor repeats.

use std::path::PathBuf;

use super::runner::CommandRunner;
use super::sim_recovery::AgentExitOutcome;

/// The longest part of an agent's log line the supervisor repeats, in
/// characters. The watchdog's report is about 250; the cap keeps a stray long
/// line (a SIP message in an error) from flooding the journal.
const MAX_QUOTED_CHARS: usize = 400;

/// The most of a run's log that is kept, in bytes: its end, from a line start.
/// `/tmp` is usually a tmpfs, so each copy costs memory, and an agent that ran
/// for weeks has a log of tens of megabytes. Its last hours are what explain
/// its exit; a day of a quiet line is well under 1 MiB.
const MAX_KEPT_BYTES: usize = 4 * 1024 * 1024;

/// Put in front of a kept log that was cut to [`MAX_KEPT_BYTES`].
const CUT_NOTE: &str = "[supervise] the start of this run's log was not kept\n";

/// How the agent's closing error line starts (`ims::agent::run`).
const ERROR_PREFIX: &str = "error: ";

/// The log of line `idx`'s most recent agent run that has ended.
pub fn previous_run_log(idx: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/ims-agent-{idx}.prev.out"))
}

/// The log of line `idx`'s most recent agent run the stall watchdog stopped.
pub fn stalled_run_log(idx: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/ims-agent-{idx}.stall.out"))
}

/// Keeps the log of a run that has just exited and returns the lines to print
/// about it. Call it after the exit has been classified and before the next
/// start empties the agent's log. A copy that cannot be written is reported in
/// the returned lines rather than stopping the restart.
pub fn keep_exited_run(
    runner: &dyn CommandRunner,
    idx: u32,
    log: &str,
    outcome: AgentExitOutcome,
) -> Vec<String> {
    let mut lines = Vec::new();
    let kept = kept_part(log);
    let mut keep = |path: PathBuf| {
        if let Err(e) = runner.write_file(&path, &kept) {
            lines.push(format!(
                "[supervise] line {idx}: could not keep the agent's log in {}: {e}",
                path.display()
            ));
        }
    };
    keep(previous_run_log(idx));
    let stalled = outcome == AgentExitOutcome::AtStall;
    if stalled {
        keep(stalled_run_log(idx));
    }
    if stalled {
        if let Some(report) = watchdog_report(log) {
            lines.push(format!(
                "[supervise] line {idx}: watchdog report (run kept in {}): {report}",
                stalled_run_log(idx).display()
            ));
        }
    }
    if let Some(error) = last_error(log) {
        lines.push(format!(
            "[supervise] line {idx}: the agent's last error: {error}"
        ));
    }
    lines
}

/// What is kept of `log`: all of it, or its last [`MAX_KEPT_BYTES`] from the
/// first line that starts inside them, after [`CUT_NOTE`].
fn kept_part(log: &str) -> std::borrow::Cow<'_, str> {
    if log.len() <= MAX_KEPT_BYTES {
        return log.into();
    }
    let from = log.len() - MAX_KEPT_BYTES;
    let start = match log[from..].find('\n') {
        Some(newline) => from + newline + 1,
        // One line longer than the cap: keep its end from a character start.
        None => (from..log.len())
            .find(|&i| log.is_char_boundary(i))
            .unwrap_or(log.len()),
    };
    format!("{CUT_NOTE}{}", &log[start..]).into()
}

/// The watchdog's report from a run it stopped, the last one if there are
/// several, without colour codes.
fn watchdog_report(log: &str) -> Option<String> {
    log.lines()
        .rev()
        .find(|l| l.contains(crate::ims::agent::watchdog::STALL_MARKER))
        .map(|l| quoted(&strip_ansi(l)))
}

/// The agent's closing `error: …` line, without the prefix. A run that was
/// killed, stopped by the watchdog or ended cleanly has none.
fn last_error(log: &str) -> Option<String> {
    log.lines()
        .rev()
        .find_map(|l| l.strip_prefix(ERROR_PREFIX))
        .map(|e| quoted(e.trim()))
}

/// `line` trimmed, with subscriber identifiers masked, and cut to
/// [`MAX_QUOTED_CHARS`], marking a cut with `…`.
fn quoted(line: &str) -> String {
    let line = super::orchestrate::mask_subscriber_ids(line.trim());
    match line.char_indices().nth(MAX_QUOTED_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// `line` without the ANSI colour sequences (`ESC [ … final`) the agent's
/// logger writes.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, up to and including the final byte.
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::agent::watchdog::STALL_MARKER;
    use crate::supervise::runner::MockCommandRunner;

    /// The watchdog's report as the agent's logger writes it to the file, with
    /// its colour codes (`report_stall`).
    fn stall_line() -> String {
        format!(
            "\u{1b}[2m2026-11-26T22:14:29.512Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m \
             \u{1b}[2mgsm_sip_bridge::ims::agent::watchdog\u{1b}[0m\u{1b}[2m:\u{1b}[0m \
             {STALL_MARKER} \u{1b}[3mactivity\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"sms-sweep\" \
             \u{1b}[3mlast_at_command\u{1b}[0m\u{1b}[2m=\u{1b}[0mAT+CMGL=4"
        )
    }

    fn stalled_log() -> String {
        format!(
            "\u{1b}[2m2026-11-26T22:14:00Z\u{1b}[0m  INFO registered, listening for inbound calls\n{}\n",
            stall_line()
        )
    }

    #[test]
    fn every_exit_keeps_the_run_in_the_previous_run_log() {
        let mock = MockCommandRunner::new();
        let log = "error: discovery error: AT command timeout\n";
        keep_exited_run(&mock, 0, log, AgentExitOutcome::Other);
        assert_eq!(mock.read_file(&previous_run_log(0)).unwrap(), log);
        assert!(
            mock.read_file(&stalled_run_log(0)).is_err(),
            "only a stall is kept in the stall log"
        );
    }

    #[test]
    fn a_stall_survives_the_quick_failures_after_it() {
        // 2026-11-26: a stall, then three quick failures while the modem came
        // back. The previous-run log ends with the last of them; the stall log
        // still holds the stall.
        let mock = MockCommandRunner::new();
        keep_exited_run(&mock, 0, &stalled_log(), AgentExitOutcome::AtStall);
        for _ in 0..3 {
            keep_exited_run(
                &mock,
                0,
                "error: failed to open serial /dev/ttyUSB0\n",
                AgentExitOutcome::Other,
            );
        }
        assert_eq!(mock.read_file(&stalled_run_log(0)).unwrap(), stalled_log());
        assert_eq!(
            mock.read_file(&previous_run_log(0)).unwrap(),
            "error: failed to open serial /dev/ttyUSB0\n"
        );
    }

    #[test]
    fn a_stall_prints_the_watchdog_report_without_colour_codes() {
        let mock = MockCommandRunner::new();
        let lines = keep_exited_run(&mock, 0, &stalled_log(), AgentExitOutcome::AtStall);
        assert_eq!(
            lines,
            [format!(
                "[supervise] line 0: watchdog report (run kept in /tmp/ims-agent-0.stall.out): \
                 2026-11-26T22:14:29.512Z ERROR gsm_sip_bridge::ims::agent::watchdog: \
                 {STALL_MARKER} activity=\"sms-sweep\" last_at_command=AT+CMGL=4"
            )]
        );
    }

    #[test]
    fn the_report_is_the_line_the_supervisor_classified_on() {
        // The supervisor calls a run a stall when its log carries the marker;
        // the line printed must be one of those.
        let log = stalled_log();
        assert_eq!(
            crate::supervise::sim_recovery::classify_exit(&log, false),
            AgentExitOutcome::AtStall
        );
        assert!(watchdog_report(&log).unwrap().contains(STALL_MARKER));
    }

    #[test]
    fn a_failed_run_prints_its_last_error() {
        let mock = MockCommandRunner::new();
        let log = "\u{1b}[33m WARN\u{1b}[0m something earlier\n\
                   error: an earlier error the agent survived\n\
                   error: discovery error: <port> did not answer its sync within 5000 ms\n";
        let lines = keep_exited_run(&mock, 1, log, AgentExitOutcome::Other);
        assert_eq!(
            lines,
            ["[supervise] line 1: the agent's last error: discovery error: <port> did not answer \
              its sync within 5000 ms"]
        );
    }

    #[test]
    fn a_killed_run_prints_nothing() {
        let mock = MockCommandRunner::new();
        let log = "\u{1b}[32m INFO\u{1b}[0m registered, listening for inbound calls\n";
        assert!(keep_exited_run(&mock, 0, log, AgentExitOutcome::Other).is_empty());
    }

    #[test]
    fn the_report_is_printed_only_for_a_run_classified_as_a_stall() {
        // The marker is a fact about the log; the outcome is the supervisor's
        // verdict on the exit, and that is what decides.
        let mock = MockCommandRunner::new();
        let lines = keep_exited_run(&mock, 0, &stalled_log(), AgentExitOutcome::Other);
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn a_long_line_is_cut() {
        let long = "x".repeat(MAX_QUOTED_CHARS + 50);
        let error = last_error(&format!("error: {long}\n")).unwrap();
        assert_eq!(error.chars().count(), MAX_QUOTED_CHARS + 1);
        assert!(error.ends_with('…'));
        let exact = "y".repeat(MAX_QUOTED_CHARS);
        assert_eq!(last_error(&format!("error: {exact}\n")).unwrap(), exact);
    }

    #[test]
    fn a_cut_respects_multibyte_characters() {
        let long = "й".repeat(MAX_QUOTED_CHARS + 1);
        let error = last_error(&format!("error: {long}\n")).unwrap();
        assert_eq!(error.chars().count(), MAX_QUOTED_CHARS + 1);
    }

    #[test]
    fn a_copy_that_cannot_be_written_is_reported_and_the_rest_still_happens() {
        let mock = MockCommandRunner::new();
        mock.failing_writes
            .lock()
            .unwrap()
            .push(previous_run_log(0));
        let lines = keep_exited_run(&mock, 0, &stalled_log(), AgentExitOutcome::AtStall);
        assert!(
            lines[0].starts_with(
                "[supervise] line 0: could not keep the agent's log in /tmp/ims-agent-0.prev.out:"
            ),
            "{lines:?}"
        );
        assert!(lines[1].contains("watchdog report"), "{lines:?}");
        assert_eq!(mock.read_file(&stalled_run_log(0)).unwrap(), stalled_log());
    }

    #[test]
    fn subscriber_identifiers_are_masked_in_what_is_printed() {
        // A synthetic test-network IMSI (MCC 001, MNC 01).
        let error = last_error("error: no SIM with IMSI 001010123456789 in the reader\n").unwrap();
        assert_eq!(error, "no SIM with IMSI 00101XXXXXXXXXX in the reader");
    }

    #[test]
    fn a_short_log_is_kept_whole() {
        let log = "one\ntwo\n";
        assert_eq!(kept_part(log), log);
    }

    #[test]
    fn a_long_log_keeps_its_end_from_a_line_start() {
        let line = "0123456789abcdef\n";
        let log = line.repeat(MAX_KEPT_BYTES / line.len() + 10) + "the last line\n";
        let kept = kept_part(&log);
        let body = kept.strip_prefix(CUT_NOTE).expect("a cut log says so");
        assert!(body.len() <= MAX_KEPT_BYTES);
        assert!(
            body.starts_with(line),
            "starts at a line: {:?}",
            &body[..20]
        );
        assert!(body.ends_with("the last line\n"));
        assert!(log.ends_with(body));
    }

    #[test]
    fn a_single_line_longer_than_the_cap_keeps_its_end() {
        let log = "й".repeat(MAX_KEPT_BYTES);
        let kept = kept_part(&log);
        let body = kept.strip_prefix(CUT_NOTE).expect("a cut log says so");
        assert!(body.len() <= MAX_KEPT_BYTES);
        assert!(log.ends_with(body));
    }

    #[test]
    fn colour_codes_are_removed_and_other_text_kept() {
        assert_eq!(
            strip_ansi("\u{1b}[2mtime\u{1b}[0m \u{1b}[1;31mERROR\u{1b}[0m é [not a code]"),
            "time ERROR é [not a code]"
        );
        assert_eq!(strip_ansi("lone \u{1b} escape"), "lone \u{1b} escape");
    }
}
