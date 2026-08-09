//! specs/030-bad-port-isolation: the operator blocklist, the in-memory
//! per-port quarantine, and the abandonable bounded worker both phases of the
//! scan run their I/O on.
//!
//! Split out of `discovery::mod` because this is the only stateful piece of
//! discovery — everything else in the scan is a pure function of sysfs plus
//! whatever the modem answers. It is also entirely testable without hardware,
//! which the rest of the probe path is not.

use super::sysfs::CandidatePort;
use crate::config::DiscoveryConfig;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Consecutive per-port probe timeouts after which a port is quarantined in
/// memory for the process lifetime (specs/030-bad-port-isolation FR-013).
pub(super) const QUARANTINE_THRESHOLD: u8 = 3;

/// The config-driven filtering plus the in-memory quarantine bookkeeping that a
/// scan consults (specs/030-bad-port-isolation). The quarantine must persist
/// *across* rescans but not across process restart, so a long-lived caller (the
/// `CardPool` rescan loop) owns one `DiscoveryPolicy` and threads `&mut` into
/// each scan; one-shot scans build a transient one.
pub struct DiscoveryPolicy {
    config: DiscoveryConfig,
    /// Consecutive AT-open-probe timeouts, keyed by the stable USB-topology
    /// interface path — NOT the `/dev/ttyUSB*` device path, which is reused
    /// across replug (a device-name-keyed quarantine would skip a healthy modem
    /// that inherited a failed one's number). Reset on any non-timeout AT
    /// result.
    consecutive_at_timeouts: HashMap<PathBuf, u8>,
    /// Consecutive SIM-status-read timeouts, keyed the same way and kept
    /// SEPARATE from the AT counter on purpose: a port can answer `AT` on every
    /// rescan yet hang on `AT+CPIN?`/`AT+CIMI` each time, so the per-rescan AT
    /// success must not keep resetting a streak that needs to accumulate —
    /// otherwise the abandoned SIM-probe workers leak without bound. Reset on
    /// any completed SIM read.
    consecutive_sim_timeouts: HashMap<PathBuf, u8>,
    /// Interface paths that reached `QUARANTINE_THRESHOLD` consecutive timeouts
    /// of either phase — skipped (never opened) by later scans for the process
    /// lifetime.
    quarantined: HashSet<PathBuf>,
}

impl DiscoveryPolicy {
    pub fn new(config: DiscoveryConfig) -> Self {
        Self {
            config,
            consecutive_at_timeouts: HashMap::new(),
            consecutive_sim_timeouts: HashMap::new(),
            quarantined: HashSet::new(),
        }
    }

    /// A policy that excludes nothing and uses the default probe timeout — for
    /// tests and the one-shot/legacy scan paths that carry no operator config.
    /// The bounded probe (and thus the wedge protection, FR-001) is still fully
    /// active, since the default timeout is baked into `DiscoveryConfig`.
    pub fn unfiltered() -> Self {
        Self::new(DiscoveryConfig::default())
    }

    /// How long a single bounded probe may run before it is abandoned.
    pub(super) fn probe_timeout(&self) -> Duration {
        self.config.probe_timeout
    }

    pub(super) fn is_blocklisted(&self, port: &CandidatePort) -> bool {
        self.config
            .excluded
            .iter()
            .any(|m| m.matches(&port.device_path, &port.iface_path))
    }

    /// Whether the interface at `iface_path` (the stable topology path) is
    /// quarantined.
    pub(super) fn is_quarantined(&self, iface_path: &Path) -> bool {
        self.quarantined.contains(iface_path)
    }

    /// Records an AT-open-probe timeout for `iface_path`; quarantines it once it
    /// has done so `QUARANTINE_THRESHOLD` times in a row. Returns `true` only on
    /// the scan that first crosses the threshold, so the caller can emit a
    /// one-time transition warning — after that the port is silently skipped, so
    /// without that log the quarantine would leave no trace.
    pub(super) fn record_at_timeout(&mut self, iface_path: &Path) -> bool {
        Self::bump(
            &mut self.consecutive_at_timeouts,
            &mut self.quarantined,
            iface_path,
        )
    }

    /// Records a completed AT probe (any non-timeout result) for `iface_path`,
    /// resetting its AT-timeout streak.
    pub(super) fn record_at_responded(&mut self, iface_path: &Path) {
        self.consecutive_at_timeouts.remove(iface_path);
    }

    /// Records a SIM-status-read timeout for `iface_path`; quarantines after
    /// `QUARANTINE_THRESHOLD` in a row (bounding the SIM-probe workers a
    /// persistently SIM-hanging port would otherwise leak). Returns `true` only
    /// on the crossing scan.
    pub(super) fn record_sim_timeout(&mut self, iface_path: &Path) -> bool {
        Self::bump(
            &mut self.consecutive_sim_timeouts,
            &mut self.quarantined,
            iface_path,
        )
    }

    /// Records a completed SIM read (any non-timeout result) for `iface_path`,
    /// resetting its SIM-timeout streak.
    pub(super) fn record_sim_responded(&mut self, iface_path: &Path) {
        self.consecutive_sim_timeouts.remove(iface_path);
    }

    /// Increments a phase counter and quarantines `iface_path` once it reaches
    /// the threshold. Returns `true` only on the crossing. Takes the two fields
    /// as disjoint borrows so it can be shared by both phases' record methods.
    fn bump(
        counters: &mut HashMap<PathBuf, u8>,
        quarantined: &mut HashSet<PathBuf>,
        iface_path: &Path,
    ) -> bool {
        let counter = counters.entry(iface_path.to_path_buf()).or_insert(0);
        *counter += 1;
        if *counter >= QUARANTINE_THRESHOLD {
            // `HashSet::insert` returns true only when newly inserted — exactly
            // the crossing event.
            quarantined.insert(iface_path.to_path_buf())
        } else {
            false
        }
    }

    /// The current AT-timeout streak for `iface_path`. Test-only: the streak is
    /// an implementation detail of the quarantine, but `probe`'s selection tests
    /// need to assert that an abandoned port actually took a strike.
    #[cfg(test)]
    pub(super) fn at_timeout_streak(&self, iface_path: &Path) -> Option<u8> {
        self.consecutive_at_timeouts.get(iface_path).copied()
    }
}

/// Runs `work` on a throwaway thread and waits at most `timeout` for it.
/// Returns `None` if it did not finish in time — the worker is then
/// deliberately leaked. A serial `open`/read on a port that wedges the kernel
/// `option` driver is uninterruptible from user space (a userspace read-timeout
/// and even `SIGTERM` don't break it), so abandoning the worker is the only way
/// to keep the scan moving (specs/030-bad-port-isolation). The leaked thread
/// stays blocked for the process lifetime — bounded by the per-port quarantine
/// and the operator blocklist. Same bounded-`recv_timeout` idiom already used in
/// `ims/agent.rs` and `observability/reporter.rs`.
pub(super) fn run_bounded<T, F>(timeout: Duration, work: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A send error just means the scan already gave up and dropped the
        // receiver; nothing to do but let this thread end (or stay blocked in
        // the kernel, if that is why we were abandoned).
        let _ = tx.send(work());
    });
    rx.recv_timeout(timeout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- specs/030-bad-port-isolation: bounded probe, quarantine, blocklist.
    // The real kernel hang needs the specific hardware; a never-returning
    // closure is the faithful stand-in for "an open/read that never comes
    // back", exercising the actual thread-spawn + recv_timeout mechanism. ---

    #[test]
    fn run_bounded_abandons_work_that_never_finishes() {
        let start = std::time::Instant::now();
        let result: Option<()> = run_bounded(Duration::from_millis(150), || {
            // Stands in for a serial open that wedges the kernel driver.
            std::thread::sleep(Duration::from_secs(3600));
        });
        assert!(
            result.is_none(),
            "a never-finishing probe must be abandoned"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "abandoning must happen at ~the timeout, not wait for the work"
        );
    }

    #[test]
    fn run_bounded_returns_a_slow_but_healthy_result() {
        // Sleeps well under the timeout: a slow-but-working port must resolve,
        // not be falsely abandoned (US1 acceptance scenario 3).
        let result = run_bounded(Duration::from_secs(2), || {
            std::thread::sleep(Duration::from_millis(100));
            42
        });
        assert_eq!(result, Some(42));
    }

    #[test]
    fn port_is_quarantined_after_three_consecutive_timeouts() {
        let mut policy = DiscoveryPolicy::unfiltered();
        let iface = Path::new("5-1:1.1");
        assert!(!policy.is_quarantined(iface));
        policy.record_at_timeout(iface);
        policy.record_at_timeout(iface);
        assert!(!policy.is_quarantined(iface), "only two timeouts so far");
        assert!(
            policy.record_at_timeout(iface),
            "the third crossing returns true"
        );
        assert!(
            policy.is_quarantined(iface),
            "quarantined on the third in a row"
        );
    }

    #[test]
    fn a_responding_probe_resets_the_timeout_streak() {
        let mut policy = DiscoveryPolicy::unfiltered();
        let iface = Path::new("5-1:1.1");
        policy.record_at_timeout(iface);
        policy.record_at_timeout(iface);
        policy.record_at_responded(iface); // streak broken by a real result
        policy.record_at_timeout(iface);
        policy.record_at_timeout(iface);
        assert!(
            !policy.is_quarantined(iface),
            "two, a reset, then two more must not reach the threshold"
        );
    }

    #[test]
    fn blocklist_matches_device_prefix_and_leaves_others_alone() {
        use crate::config::{DiscoveryConfig, PortMatcher};
        let config = DiscoveryConfig {
            excluded: vec![PortMatcher::parse("5-1.2.1.2").unwrap()],
            ..DiscoveryConfig::default()
        };
        let policy = DiscoveryPolicy::new(config);
        let excluded = CandidatePort {
            device_path: PathBuf::from("/dev/ttyUSB1"),
            iface_path: PathBuf::from("/sys/bus/usb/devices/5-1.2.1.2:1.1"),
        };
        let other = CandidatePort {
            device_path: PathBuf::from("/dev/ttyUSB0"),
            iface_path: PathBuf::from("/sys/bus/usb/devices/5-1.2.1.3:1.0"),
        };
        assert!(
            policy.is_blocklisted(&excluded),
            "a whole-device topology fragment excludes its interfaces"
        );
        assert!(
            !policy.is_blocklisted(&other),
            "a different device is untouched"
        );
    }

    #[test]
    fn multiple_bad_ports_are_tracked_and_quarantined_independently() {
        // Edge case: several simultaneously-wedged interfaces must each
        // accumulate their own streak, so one hitting the threshold never
        // quarantines an unrelated one.
        let mut policy = DiscoveryPolicy::unfiltered();
        let a = Path::new("5-1:1.1");
        let b = Path::new("5-1:1.2");
        for _ in 0..QUARANTINE_THRESHOLD {
            policy.record_at_timeout(a);
        }
        policy.record_at_timeout(b);
        assert!(policy.is_quarantined(a), "the port that hit the threshold");
        assert!(
            !policy.is_quarantined(b),
            "one timeout must not quarantine a different port"
        );
    }

    #[test]
    fn unfiltered_policy_excludes_nothing_and_uses_the_default_timeout() {
        let policy = DiscoveryPolicy::unfiltered();
        let port = CandidatePort {
            device_path: PathBuf::from("/dev/ttyUSB1"),
            iface_path: PathBuf::from("/sys/bus/usb/devices/5-1.2.1.2:1.1"),
        };
        assert!(
            !policy.is_blocklisted(&port),
            "an empty [discovery] must exclude nothing (FR-008)"
        );
        assert_eq!(
            policy.probe_timeout(),
            Duration::from_millis(crate::config::DEFAULT_PROBE_TIMEOUT_MS)
        );
    }

    #[test]
    fn sim_read_timeouts_quarantine_after_three_in_a_row() {
        // P1-A: a port that answers AT but hangs on the SIM read every rescan
        // would otherwise leak an abandoned worker forever. A run of SIM-read
        // timeouts must reach quarantine (via its own counter) to bound that.
        let mut policy = DiscoveryPolicy::unfiltered();
        let iface = Path::new("5-1:1.1");
        assert!(!policy.record_sim_timeout(iface));
        assert!(!policy.record_sim_timeout(iface));
        assert!(
            policy.record_sim_timeout(iface),
            "the third consecutive SIM-read timeout quarantines"
        );
        assert!(policy.is_quarantined(iface));
    }

    #[test]
    fn a_completed_sim_read_resets_the_sim_timeout_streak() {
        // A merely-slow-but-healthy modem: an occasional SIM timeout that is
        // followed by a good read must never reach the threshold.
        let mut policy = DiscoveryPolicy::unfiltered();
        let iface = Path::new("5-1:1.1");
        policy.record_sim_timeout(iface);
        policy.record_sim_timeout(iface);
        policy.record_sim_responded(iface);
        policy.record_sim_timeout(iface);
        policy.record_sim_timeout(iface);
        assert!(
            !policy.is_quarantined(iface),
            "the reset prevents reaching the threshold"
        );
    }

    #[test]
    fn at_probe_success_does_not_reset_the_sim_timeout_streak() {
        // The whole point of a SEPARATE SIM counter (P1-A): a port that answers
        // AT on every rescan (resetting the AT streak) but hangs on the SIM read
        // each time must still accumulate toward quarantine.
        let mut policy = DiscoveryPolicy::unfiltered();
        let iface = Path::new("5-1:1.1");
        policy.record_sim_timeout(iface);
        policy.record_at_responded(iface); // AT-open success on the next rescan
        policy.record_sim_timeout(iface);
        policy.record_at_responded(iface);
        assert!(
            policy.record_sim_timeout(iface),
            "AT success must not reset the SIM streak; the 3rd SIM timeout quarantines"
        );
        assert!(policy.is_quarantined(iface));
    }
}
