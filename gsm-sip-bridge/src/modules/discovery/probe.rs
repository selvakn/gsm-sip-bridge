//! Finding which of a modem's serial interfaces actually answers `AT`.
//!
//! Split out of `discovery::mod` because the *selection* logic here
//! ([`select_at_capable_port`]) is the part specs/030-bad-port-isolation made
//! testable — it takes the prober as a parameter, so the blocklist/quarantine
//! skips and the abandon-and-continue behavior are all exercised without a
//! serial port. Only [`probe_one_candidate`] touches real hardware.

use super::policy::{run_bounded, DiscoveryPolicy, QUARANTINE_THRESHOLD};
use super::sysfs::{candidate_tty_ports, CandidatePort};
use crate::modules::at_commander::{AtCommander, AtResponse};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Per-candidate timeout for the AT probe (specs/013-multi-card-vowifi
/// FR-002) — short because a modem may expose several serial interfaces
/// that are never going to answer AT (diagnostic/NMEA ports), and probing
/// tries each one in turn.
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// The result of probing one candidate serial interface
/// (specs/030-bad-port-isolation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    /// Answered `AT` with `OK` — usable.
    AtCapable,
    /// A real, non-timeout result: opened but not `AT`-capable, or the open
    /// itself failed cleanly. Either way the port responded, so it resets the
    /// consecutive-timeout streak.
    NotAtCapable,
    /// The bounded probe was abandoned because it did not finish in time.
    TimedOut,
}

/// Tries every candidate serial interface in turn (an operator-preferred
/// one first, if present — see `order_candidates_with_preference`), opening
/// it and sending a bare `AT`, and returns the first one that answers `OK` —
/// the live probe replacing the old fixed-interface-number lookup (FR-002).
///
/// Each candidate the operator excluded (FR-007) or that has been quarantined
/// after repeated timeouts (FR-013) is skipped without opening it. Every open +
/// AT exchange runs on an abandonable worker bounded by `policy`'s probe
/// timeout, so a port that wedges the kernel driver is abandoned rather than
/// wedging the whole scan (FR-001/FR-002). Real hardware I/O; the bounded-runner
/// mechanism, the matcher, and the quarantine counter are unit-tested.
pub(super) fn probe_at_port(
    dev_path: &Path,
    preferred: &[PathBuf],
    policy: &mut DiscoveryPolicy,
) -> Option<CandidatePort> {
    let candidates = order_candidates_with_preference(candidate_tty_ports(dev_path), preferred);
    select_at_capable_port(candidates, policy, probe_one_candidate)
}

/// Reorders `candidates` so any whose device path appears in `preferred` come
/// first (each in its original relative order otherwise) — a device with
/// several AT-capable interfaces should try an operator-named port before
/// falling through to "whichever answers first" (see `scan_all_preferring`'s
/// doc comment). Pure and unit-tested; `probe_at_port` (real serial I/O) is not.
fn order_candidates_with_preference(
    candidates: Vec<CandidatePort>,
    preferred: &[PathBuf],
) -> Vec<CandidatePort> {
    let (mut first, mut rest): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| preferred.contains(&c.device_path));
    first.append(&mut rest);
    first
}

/// The candidate-selection logic, factored out of live serial I/O so it is
/// unit-testable with a fake `probe_one`: it applies the blocklist and
/// quarantine skips (so an excluded/quarantined port is *never handed to*
/// `probe_one`, FR-007/FR-013/SC-003), calls `probe_one` for each remaining
/// candidate in order, updates the quarantine bookkeeping (keyed by the stable
/// topology iface path), logs, and returns the first `AT`-capable candidate —
/// continuing past an abandoned one (FR-002/FR-003). Production passes
/// [`probe_one_candidate`] (real bounded serial I/O); tests pass a scripted
/// closure.
fn select_at_capable_port(
    candidates: Vec<CandidatePort>,
    policy: &mut DiscoveryPolicy,
    mut probe_one: impl FnMut(&Path, Duration) -> ProbeOutcome,
) -> Option<CandidatePort> {
    let timeout = policy.probe_timeout();
    for candidate in candidates {
        if policy.is_blocklisted(&candidate) {
            // info, not debug: US3 scenario 2 — an operator reading normal logs
            // must be able to see that their exclusion is taking effect.
            tracing::info!(
                port = %candidate.device_path.display(),
                iface = %candidate.iface_path.display(),
                "serial port skipped by the [discovery].excluded_ports exclusion list; not probing"
            );
            continue;
        }
        if policy.is_quarantined(&candidate.iface_path) {
            tracing::debug!(
                port = %candidate.device_path.display(),
                iface = %candidate.iface_path.display(),
                "serial port quarantined after repeated probe timeouts; not probed again until \
                 process restart"
            );
            continue;
        }

        match probe_one(&candidate.device_path, timeout) {
            ProbeOutcome::AtCapable => {
                policy.record_at_responded(&candidate.iface_path);
                return Some(candidate);
            }
            ProbeOutcome::NotAtCapable => policy.record_at_responded(&candidate.iface_path),
            ProbeOutcome::TimedOut => {
                let newly_quarantined = policy.record_at_timeout(&candidate.iface_path);
                tracing::warn!(
                    port = %candidate.device_path.display(),
                    iface = %candidate.iface_path.display(),
                    timeout_ms = timeout.as_millis(),
                    "AT probe exceeded timeout; abandoning port, left unresolved \
                     (add its iface path to [discovery].excluded_ports to skip it permanently)"
                );
                if newly_quarantined {
                    // One-time transition record: after this the port is only
                    // skipped at debug, so this is the durable evidence.
                    tracing::warn!(
                        port = %candidate.device_path.display(),
                        iface = %candidate.iface_path.display(),
                        threshold = QUARANTINE_THRESHOLD,
                        "serial port quarantined for the process lifetime after consecutive probe \
                         timeouts; it will not be probed again until restart — add its iface path \
                         to [discovery].excluded_ports to make this permanent"
                    );
                }
            }
        }
    }
    None
}

/// Production `probe_one`: opens the port and sends a bare `AT` on an
/// abandonable worker bounded by `timeout` (see `run_bounded`). Real hardware
/// I/O — the surrounding selection logic ([`select_at_capable_port`]) is what's
/// unit-tested.
fn probe_one_candidate(device_path: &Path, timeout: Duration) -> ProbeOutcome {
    let probe_path = device_path.to_path_buf();
    match run_bounded(timeout, move || {
        match AtCommander::open_with_timeout(&probe_path, PROBE_TIMEOUT) {
            Ok(mut at) => probe_is_at_capable(&mut at),
            Err(e) => {
                tracing::debug!(
                    port = %probe_path.display(),
                    error = %e,
                    "could not open candidate serial port during AT probe"
                );
                false
            }
        }
    }) {
        Some(true) => ProbeOutcome::AtCapable,
        Some(false) => ProbeOutcome::NotAtCapable,
        None => ProbeOutcome::TimedOut,
    }
}

/// Sends a bare `AT` and returns whether the device answered with a
/// well-formed response (`OK`) — the core of the AT-probe (FR-002). Takes
/// an already-open `AtCommander`, so it's exercised in tests against a fake
/// in-memory transport (mirroring `at_commander.rs`'s own `MockStream`)
/// without touching real hardware. Private: it used to be `pub` only so the
/// single module-wide test block could reach it, and that block now lives
/// here.
fn probe_is_at_capable(at: &mut AtCommander) -> bool {
    matches!(at.send_command("AT"), Ok(AtResponse::Ok(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::discovery::test_support::make_commander;
    use std::collections::HashMap;

    /// Builds a `CandidatePort` from a device path and an interface (topology)
    /// fragment, for the ordering/matching tests below.
    fn cand(dev: &str, iface: &str) -> CandidatePort {
        CandidatePort {
            device_path: PathBuf::from(dev),
            iface_path: PathBuf::from(iface),
        }
    }

    fn device_paths(cands: Vec<CandidatePort>) -> Vec<PathBuf> {
        cands.into_iter().map(|c| c.device_path).collect()
    }

    /// A scripted `probe_one` for `select_at_capable_port`: maps each device
    /// path to a fixed outcome and records the order in which ports are actually
    /// handed to it — so a test can assert both the selection result and that a
    /// blocklisted/quarantined port is *never probed* (SC-003). A `TimedOut`
    /// entry is the fake-port stand-in for an open that never returns.
    fn scripted_probe(
        outcomes: HashMap<PathBuf, ProbeOutcome>,
        probed: std::rc::Rc<std::cell::RefCell<Vec<PathBuf>>>,
    ) -> impl FnMut(&Path, Duration) -> ProbeOutcome {
        move |port: &Path, _timeout| {
            probed.borrow_mut().push(port.to_path_buf());
            outcomes
                .get(port)
                .copied()
                .unwrap_or(ProbeOutcome::NotAtCapable)
        }
    }

    #[test]
    fn probe_abandons_a_wedged_candidate_and_continues_to_the_next() {
        let bad = PathBuf::from("/dev/ttyUSB1");
        let good = PathBuf::from("/dev/ttyUSB2");
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1:1.1"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let outcomes = HashMap::from([
            (bad.clone(), ProbeOutcome::TimedOut),
            (good.clone(), ProbeOutcome::AtCapable),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut policy = DiscoveryPolicy::unfiltered();
        let result = select_at_capable_port(
            candidates,
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(
            result.map(|c| c.device_path),
            Some(good.clone()),
            "abandons the wedged candidate and returns the next AT-capable one"
        );
        assert_eq!(*probed.borrow(), vec![bad, good], "both tried, in order");
        assert_eq!(
            policy.at_timeout_streak(Path::new("5-1:1.1")),
            Some(1),
            "the abandoned port took a timeout strike, keyed by its topology iface path"
        );
    }

    #[test]
    fn probe_returns_none_when_every_candidate_times_out() {
        // FR-011 / T010: a modem whose only interfaces all wedge yields no
        // usable AT port (and does not hang).
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1:1.1"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let outcomes = HashMap::from([
            (PathBuf::from("/dev/ttyUSB1"), ProbeOutcome::TimedOut),
            (PathBuf::from("/dev/ttyUSB2"), ProbeOutcome::TimedOut),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let mut policy = DiscoveryPolicy::unfiltered();
        let result =
            select_at_capable_port(candidates, &mut policy, scripted_probe(outcomes, probed));
        assert_eq!(result, None);
    }

    #[test]
    fn probe_never_opens_a_blocklisted_port() {
        use crate::config::{DiscoveryConfig, PortMatcher};
        // ttyUSB1 is blocklisted; even though the fake would answer AT on it, it
        // must never be handed to the prober (SC-003), so the healthy ttyUSB2
        // wins.
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1.2.1.2:1.1"),
            cand("/dev/ttyUSB2", "5-1.2.1.3:1.0"),
        ];
        let config = DiscoveryConfig {
            excluded: vec![PortMatcher::parse("5-1.2.1.2:1.1").unwrap()],
            ..DiscoveryConfig::default()
        };
        let mut policy = DiscoveryPolicy::new(config);
        let outcomes = HashMap::from([
            (PathBuf::from("/dev/ttyUSB1"), ProbeOutcome::AtCapable),
            (PathBuf::from("/dev/ttyUSB2"), ProbeOutcome::AtCapable),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result = select_at_capable_port(
            candidates,
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(
            result.map(|c| c.device_path),
            Some(PathBuf::from("/dev/ttyUSB2"))
        );
        assert!(
            !probed.borrow().contains(&PathBuf::from("/dev/ttyUSB1")),
            "a blocklisted port is never opened/probed (SC-003)"
        );
    }

    #[test]
    fn probe_skips_a_quarantined_port() {
        let candidates = vec![
            cand("/dev/ttyUSB1", "5-1:1.1"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let mut policy = DiscoveryPolicy::unfiltered();
        // Quarantine ttyUSB1 by its stable topology iface path (P1-B: NOT the
        // device path, which is reused across replug).
        for _ in 0..QUARANTINE_THRESHOLD {
            policy.record_at_timeout(Path::new("5-1:1.1"));
        }
        let outcomes = HashMap::from([
            (PathBuf::from("/dev/ttyUSB1"), ProbeOutcome::AtCapable),
            (PathBuf::from("/dev/ttyUSB2"), ProbeOutcome::AtCapable),
        ]);
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let result = select_at_capable_port(
            candidates,
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(
            result.map(|c| c.device_path),
            Some(PathBuf::from("/dev/ttyUSB2"))
        );
        assert!(
            !probed.borrow().contains(&PathBuf::from("/dev/ttyUSB1")),
            "a quarantined port is not re-probed on a later scan"
        );
    }

    #[test]
    fn quarantine_is_keyed_by_topology_not_device_path() {
        // P1-B: a quarantined interface must stay pinned to its USB-topology
        // path, so a healthy modem that later inherits the failed one's reused
        // /dev/ttyUSB number is NOT wrongly skipped.
        let mut policy = DiscoveryPolicy::unfiltered();
        for _ in 0..QUARANTINE_THRESHOLD {
            policy.record_at_timeout(Path::new("5-1.2.1.2:1.1"));
        }
        // Same device path, different (healthy) topology position after replug.
        let replacement = cand("/dev/ttyUSB1", "5-1.2.1.3:1.0");
        let probed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let outcomes = HashMap::from([(PathBuf::from("/dev/ttyUSB1"), ProbeOutcome::AtCapable)]);
        let result = select_at_capable_port(
            vec![replacement],
            &mut policy,
            scripted_probe(outcomes, probed.clone()),
        );
        assert_eq!(
            result.map(|c| c.device_path),
            Some(PathBuf::from("/dev/ttyUSB1")),
            "the healthy replacement at a different topology position must be probed"
        );
        assert!(probed.borrow().contains(&PathBuf::from("/dev/ttyUSB1")));
    }

    #[test]
    fn order_candidates_prefers_configured_port_when_present() {
        // Found live-testing: a real EC200 answered AT on both ttyUSB0 and
        // ttyUSB6. An operator-configured port must win over "whichever
        // sorts first" so an existing single-line config naming a
        // non-default AT port still gets used as-is (FR-009/FR-020).
        let candidates = vec![
            cand("/dev/ttyUSB0", "5-1:1.0"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
            cand("/dev/ttyUSB6", "5-1:1.6"),
        ];
        let preferred = vec![PathBuf::from("/dev/ttyUSB6")];
        assert_eq!(
            device_paths(order_candidates_with_preference(candidates, &preferred)),
            vec![
                PathBuf::from("/dev/ttyUSB6"),
                PathBuf::from("/dev/ttyUSB0"),
                PathBuf::from("/dev/ttyUSB2"),
            ]
        );
    }

    #[test]
    fn order_candidates_unchanged_when_no_preference_matches() {
        let candidates = vec![
            cand("/dev/ttyUSB0", "5-1:1.0"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        let preferred = vec![PathBuf::from("/dev/ttyUSB9")];
        assert_eq!(
            device_paths(order_candidates_with_preference(
                candidates.clone(),
                &preferred
            )),
            device_paths(candidates)
        );
    }

    #[test]
    fn order_candidates_unchanged_when_no_preference_given() {
        let candidates = vec![
            cand("/dev/ttyUSB0", "5-1:1.0"),
            cand("/dev/ttyUSB2", "5-1:1.2"),
        ];
        assert_eq!(
            device_paths(order_candidates_with_preference(candidates.clone(), &[])),
            device_paths(candidates)
        );
    }

    // --- probe_is_at_capable: fake in-memory transport, mirroring
    // at_commander.rs's own MockStream (no real hardware). ---

    #[test]
    fn probe_is_at_capable_true_on_ok() {
        let mut at = make_commander("OK\r\n");
        assert!(probe_is_at_capable(&mut at));
    }

    #[test]
    fn probe_is_at_capable_false_on_error() {
        let mut at = make_commander("ERROR\r\n");
        assert!(!probe_is_at_capable(&mut at));
    }

    #[test]
    fn probe_is_at_capable_false_on_cme_error() {
        let mut at = make_commander("+CME ERROR: 100\r\n");
        assert!(!probe_is_at_capable(&mut at));
    }
}
