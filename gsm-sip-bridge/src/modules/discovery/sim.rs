//! Reading a modem's SIM identity and readiness, and the one optional repair
//! the scan is allowed to attempt.
//!
//! Split out of `discovery::mod` because this is the only part of discovery
//! that can *write* to a modem ([`SimRecovery::CfunCycleOnUnreadable`] power-
//! cycles the radio), and keeping that behind its own module boundary makes
//! the narrow opt-in easier to audit.

use super::policy::{run_bounded, DiscoveryPolicy, QUARANTINE_THRESHOLD};
use super::probe::PROBE_TIMEOUT;
use crate::modules::at_commander::{AtCommander, AtResponse};
use std::path::Path;
use std::time::Duration;

/// SIM identity/readiness observed while probing a discovered modem
/// (specs/013-multi-card-vowifi FR-004/FR-006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimStatus {
    Ready { imsi: String },
    Absent,
    Locked,
    Unreadable(String),
}

/// Whether a scan may try to *repair* a modem whose SIM does not read,
/// rather than only observing that it doesn't.
///
/// This is a deliberately narrow opt-in because the repair
/// (`AT+CFUN=0` → `AT+CFUN=1`, see `recover_and_reprobe_sim`) is not a
/// read-only probe: it drops and re-acquires the modem's radio, and blocks
/// the scan for the cycle delay plus the readiness poll. Both are fine
/// exactly once, at startup, before any line is carrying traffic — which is
/// the only place it was ever meant to run and the only place it was live-
/// tested (specs/027-discover-retry-health).
///
/// They are *not* fine on `scan_modules`' ongoing rescans, which run for
/// the container's whole lifetime alongside modems that are actively
/// registered or mid-call. Those rescans reach the very same
/// `probe_sim_status_at`, so without this switch a modem whose SIM read
/// merely glitched — including a circuit-switched one carrying a call, which
/// `skip_card_ids` does not cover (it only protects *VoWiFi* lines) — would
/// have had its radio power-cycled out from under it, and every rescan would
/// stall for the poll window per unreadable modem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimRecovery {
    /// Observe and report `SimStatus::Unreadable`; never touch the radio.
    Disabled,
    /// On `Unreadable`, attempt one `AT+CFUN` cycle and re-probe.
    CfunCycleOnUnreadable,
}

/// The result of the bounded SIM-status open, carrying the still-open
/// `AtCommander` back out of the worker thread so an optional (unbounded) CFUN
/// recovery can reuse it (see `probe_sim_status_at`).
enum SimProbe {
    Opened(SimStatus, AtCommander),
    OpenFailed(String),
}

/// Opens `port` fresh and reads its SIM status — real hardware I/O, not
/// unit-tested directly; [`probe_sim_status`] carries the tested
/// interpretation logic. On `Unreadable`, attempts one CFUN power-cycle via
/// `recover_and_reprobe_sim` before giving up (specs/027-discover-retry-health
/// — see that function's doc comment for why).
pub(super) fn probe_sim_status_at(
    device_path: &Path,
    iface_path: &Path,
    sim_recovery: SimRecovery,
    policy: &mut DiscoveryPolicy,
) -> SimStatus {
    let timeout = policy.probe_timeout();
    let open_port = device_path.to_path_buf();
    // The bounded region is the open PLUS the `AT+CPIN?`/`AT+CIMI` reads inside
    // `probe_sim_status` — each of which can itself block up to the per-line
    // port read timeout, so the worst case approaches the full `timeout` budget
    // (which is why that budget is generous, ~5s, not just an open's worth).
    //
    // SIM-read timeouts feed a SEPARATE quarantine counter from the AT-open
    // probe (`record_sim_timeout`): a single/occasional one resets on the next
    // good read, so a merely-slow-but-healthy modem is never blackholed — but a
    // port that answers `AT` every rescan yet hangs on the SIM read *every* time
    // still reaches the threshold and is quarantined, which bounds the SIM-probe
    // workers it would otherwise leak forever (the AT-open success must not keep
    // resetting this streak, hence the separate counter). The optional CFUN
    // recovery below is deliberately left unbounded (specs/027): it sleeps for
    // CFUN_CYCLE_DELAY plus a poll window by design and would be falsely
    // abandoned by the probe timeout.
    let opened = run_bounded(timeout, move || {
        match AtCommander::open_with_timeout(&open_port, PROBE_TIMEOUT) {
            Ok(mut at) => {
                let status = probe_sim_status(&mut at);
                SimProbe::Opened(status, at)
            }
            Err(e) => SimProbe::OpenFailed(e.to_string()),
        }
    });

    let (status, mut at) = match opened {
        None => {
            let newly_quarantined = policy.record_sim_timeout(iface_path);
            tracing::warn!(
                port = %device_path.display(),
                iface = %iface_path.display(),
                timeout_ms = timeout.as_millis(),
                "SIM-status probe exceeded timeout; SIM left unread"
            );
            if newly_quarantined {
                tracing::warn!(
                    port = %device_path.display(),
                    iface = %iface_path.display(),
                    threshold = QUARANTINE_THRESHOLD,
                    "serial port quarantined for the process lifetime after consecutive SIM-read \
                     timeouts; it will not be probed again until restart — add its iface path to \
                     [discovery].excluded_ports to make this permanent"
                );
            }
            return SimStatus::Unreadable("SIM-status probe timed out".to_string());
        }
        Some(SimProbe::OpenFailed(e)) => {
            policy.record_sim_responded(iface_path);
            return SimStatus::Unreadable(e);
        }
        Some(SimProbe::Opened(status, at)) => {
            policy.record_sim_responded(iface_path);
            (status, at)
        }
    };

    if sim_recovery == SimRecovery::CfunCycleOnUnreadable
        && matches!(status, SimStatus::Unreadable(_))
    {
        tracing::warn!(
            port = %device_path.display(),
            reason = ?status,
            "SIM unreadable on first probe; attempting a CFUN power-cycle before giving up"
        );
        recover_and_reprobe_sim(
            &mut at,
            crate::supervise::sim_recovery::CFUN_CYCLE_DELAY,
            crate::supervise::sim_recovery::CPIN_POLL_INTERVAL,
            crate::supervise::sim_recovery::CPIN_POLL_ATTEMPTS,
        )
    } else {
        status
    }
}

/// After a first-probe `Unreadable` result, power-cycles the SIM in place
/// (`AT+CFUN=0` -> `AT+CFUN=1`) and re-probes once — the same recipe as
/// `supervise::sim_recovery::reset_modem_sim`/`vowifi::usim_bridge`'s
/// `reset_sim_in_place` (see either's doc comment for the sugam incident
/// this traces back to), driven directly over the `AtCommander` this probe
/// already has open rather than shelling out via `CommandRunner`.
/// `discover`'s probe never has a running vowifi-usim-bridge/swu-dialer
/// holder to freeze first, unlike those two call sites: it runs before any
/// per-line agent starts, so nothing else is using the port yet.
///
/// Without this, a SIM that is transiently unreadable at boot (the sugam
/// pattern: `+CME ERROR: 13`, cleared in practice by a soft radio cycle)
/// stayed permanently unreadable through `discover`'s one-shot probe — live
/// testing against real EC20 hardware (specs/027-discover-retry-health)
/// found this reachable, not just hypothetical. Timing is a parameter
/// (rather than reading the constants directly) purely so tests can drive
/// the same real AT-command sequence with near-zero delays.
fn recover_and_reprobe_sim(
    at: &mut AtCommander,
    cycle_delay: Duration,
    poll_interval: Duration,
    poll_attempts: u32,
) -> SimStatus {
    let _ = at.send_command("AT+CFUN=0");
    std::thread::sleep(cycle_delay);
    let _ = at.send_command("AT+CFUN=1");

    for _ in 0..poll_attempts {
        std::thread::sleep(poll_interval);
        if matches!(at.query_cpin(), Ok(status) if status.contains("READY")) {
            break;
        }
    }
    probe_sim_status(at)
}

/// Interprets `AT+CPIN?` (and, if ready, `AT+CIMI`) into a `SimStatus`
/// (FR-004/FR-006). Pure given an `AtCommander`, so it's exercised in tests
/// against a fake transport. Private: it used to be `pub` only so the single
/// module-wide test block could reach it, and that block now lives here.
fn probe_sim_status(at: &mut AtCommander) -> SimStatus {
    // Sends AT+CPIN? directly (rather than through `AtCommander::query_cpin`)
    // so a `+CME ERROR: 10` ("SIM not inserted", 3GPP TS 27.007) is matched
    // by its numeric code, not by re-parsing an already-stringified error.
    match at.send_command("AT+CPIN?") {
        Ok(AtResponse::Ok(lines)) => {
            let status = lines.iter().find_map(|l| {
                l.strip_prefix("+CPIN:")
                    .map(|s| s.trim().to_ascii_uppercase())
            });
            match status.as_deref() {
                Some("READY") => match at.query_imsi() {
                    Ok(imsi) => SimStatus::Ready { imsi },
                    Err(e) => SimStatus::Unreadable(e.to_string()),
                },
                Some(s) if s.contains("PIN") || s.contains("PUK") => SimStatus::Locked,
                Some(s) => SimStatus::Unreadable(format!("unexpected AT+CPIN? status: {s}")),
                None => SimStatus::Unreadable("AT+CPIN?: no status in response".to_string()),
            }
        }
        Ok(AtResponse::CmeError(10, _)) => SimStatus::Absent,
        Ok(AtResponse::Error(e)) | Ok(AtResponse::CmeError(_, e)) => SimStatus::Unreadable(e),
        Err(e) => SimStatus::Unreadable(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::discovery::test_support::{make_commander, make_scripted_commander};

    // `probe_sim_status`'s READY+IMSI path sends two AT commands
    // (AT+CPIN? then AT+CIMI) against one `AtCommander`. As documented in
    // `modules/usim.rs` (`ef_dir_record_matches_usim_aid_from_real_card`),
    // `AtCommander::read_response` builds a fresh `BufReader` per
    // `send_command` call, which over-reads and silently drops any
    // buffered-but-unconsumed bytes from a single-shot `Cursor`-backed mock
    // stream across more than one call — a pre-existing quirk unrelated to
    // this feature, not something to work around here. The two commands'
    // individual response parsing is covered directly instead:
    // `at_commander::tests::test_query_cpin_ready` and `test_query_imsi`.

    #[test]
    fn probe_sim_status_locked_on_sim_pin() {
        let mut at = make_commander("+CPIN: SIM PIN\r\nOK\r\n");
        assert_eq!(probe_sim_status(&mut at), SimStatus::Locked);
    }

    #[test]
    fn probe_sim_status_locked_on_sim_puk() {
        let mut at = make_commander("+CPIN: SIM PUK\r\nOK\r\n");
        assert_eq!(probe_sim_status(&mut at), SimStatus::Locked);
    }

    #[test]
    fn probe_sim_status_absent_on_cme_error_10() {
        let mut at = make_commander("+CME ERROR: 10\r\n");
        assert_eq!(probe_sim_status(&mut at), SimStatus::Absent);
    }

    #[test]
    fn probe_sim_status_unreadable_on_generic_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(matches!(
            probe_sim_status(&mut at),
            SimStatus::Unreadable(_)
        ));
    }

    /// specs/027-discover-retry-health: a SIM that is unreadable on the
    /// first probe but comes back `+CPIN: READY` after a soft radio cycle —
    /// the exact live-hardware finding (EC20, `+CME ERROR: 13`) that
    /// motivated `recover_and_reprobe_sim`.
    #[test]
    fn recover_and_reprobe_sim_returns_ready_after_a_successful_cfun_cycle() {
        let mut at = make_scripted_commander(&[
            "OK\r\n",                    // AT+CFUN=0
            "OK\r\n",                    // AT+CFUN=1
            "+CPIN: READY\r\nOK\r\n",    // poll attempt 1: AT+CPIN?
            "+CPIN: READY\r\nOK\r\n",    // re-probe: AT+CPIN?
            "404438083996440\r\nOK\r\n", // re-probe: AT+CIMI
        ]);
        let status = recover_and_reprobe_sim(
            &mut at,
            Duration::from_millis(1),
            Duration::from_millis(1),
            1,
        );
        assert_eq!(
            status,
            SimStatus::Ready {
                imsi: "404438083996440".to_string()
            }
        );
    }

    /// If the SIM never comes back `READY` within the poll window, the
    /// re-probe at the end still runs and (correctly) reports `Unreadable`
    /// again rather than panicking or hanging.
    #[test]
    fn recover_and_reprobe_sim_stays_unreadable_if_cpin_never_becomes_ready() {
        let mut at = make_scripted_commander(&[
            "OK\r\n",             // AT+CFUN=0
            "OK\r\n",             // AT+CFUN=1
            "+CME ERROR: 13\r\n", // poll attempt 1: AT+CPIN?
            "+CME ERROR: 13\r\n", // re-probe: AT+CPIN?
        ]);
        let status = recover_and_reprobe_sim(
            &mut at,
            Duration::from_millis(1),
            Duration::from_millis(1),
            1,
        );
        assert!(matches!(status, SimStatus::Unreadable(_)));
    }
}
