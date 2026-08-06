//! `gsm-sip-bridge healthcheck` — the container's `HEALTHCHECK` probe.
//!
//! Ported from `docker/healthcheck.sh` (71 lines of bash), the last piece of
//! orchestration still living outside the binary after
//! specs/021-entrypoint-supervise-rust moved everything else into
//! [`crate::supervise`]. The reasoning is the same one that motivated that
//! feature: the script `eval`'d two shell-env dumps from this very binary and
//! then did per-line `ip netns exec` and `/dev/tcp` probes — all effects
//! [`CommandRunner`] already abstracts and tests, wrapped in a language where
//! none of the decision logic could be tested at all.
//!
//! Behaviour is deliberately unchanged, including the parts that look odd
//! until you know why:
//!
//! - **Zero usable VoWiFi lines is healthy, not unhealthy.** The spec's own
//!   degrade clarification: the circuit-switched side (checked first) is what
//!   has to be up. A line-resolution problem is reported, not container-fatal.
//! - **Never re-runs discovery.** It reads the resolution file `supervise`
//!   already wrote at startup. Re-scanning USB/AT every 30s on the
//!   `HEALTHCHECK` interval would race the running agents holding those same
//!   serial ports open.
//! - **TCP connect, not ICMP**, to the P-CSCF: operators commonly filter ping.

use super::super::supervise::runner::{CommandRunner, RealCommandRunner};
use crate::cli::Cli;
use crate::config::load_config;
use crate::vowifi::discovery::{LineResolution, LineResolutionEntry};
use std::process::ExitCode;

/// SIP port on the P-CSCF, the thing a line's tunnel has to be able to reach
/// for the line to be worth anything.
const PCSCF_SIP_PORT: u16 = 5060;

/// Why a line is unhealthy. One variant per check the bash version did, so a
/// failure names itself rather than arriving as a bare exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineFault {
    /// The tunnel interface exists but has no address — the tunnel never came
    /// up, or came up and lost its lease.
    TunnelInterfaceHasNoAddress { iface: String },
    /// A P-CSCF address was captured, but nothing answers SIP on it through
    /// this line's namespace.
    PcscfUnreachable { addr: String },
}

impl std::fmt::Display for LineFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LineFault::TunnelInterfaceHasNoAddress { iface } => {
                write!(f, "tunnel interface {iface} has no address")
            }
            LineFault::PcscfUnreachable { addr } => write!(f, "P-CSCF {addr} unreachable"),
        }
    }
}

/// The whole probe's verdict, so the decision is inspectable in tests rather
/// than only observable as a process exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    /// specs/026-disable-circuit-switched FR-018: `[cs].enabled` is false and
    /// there was nothing else to check (no VoWiFi lines in play) — reported
    /// distinctly from `Healthy` so an operator reading the verdict can tell
    /// "disabled on purpose" apart from "checked and found healthy", but
    /// exits the same way (`ExitCode::SUCCESS`): a deliberately disabled
    /// path is never unhealthy, degraded, or failed.
    CircuitSwitchedDisabled,
    /// The circuit-switched daemon's metrics endpoint did not respond. Fatal
    /// on its own — nothing else is checked.
    MetricsEndpointDown,
    /// One or more VoWiFi lines failed their checks, reported per line, and/
    /// or one or more explicitly configured lines never resolved into a
    /// line at all (specs/027-discover-retry-health FR-008) — kept as a
    /// separate list from `line_faults` since a never-discovered line has
    /// no `index` to key a fault by; its identifier is whatever
    /// `[[vowifi.line]]` was pinned to (`modem_port`/`modem_serial`).
    LinesUnhealthy {
        line_faults: Vec<(u32, LineFault)>,
        configured_not_found: Vec<String>,
    },
}

impl Health {
    pub fn is_healthy(&self) -> bool {
        matches!(self, Health::Healthy | Health::CircuitSwitchedDisabled)
    }
}

/// Which tunnel interface a line's address check should look at.
///
/// For the `swu` engine this is always `tun1`, named by the SWu-IKEv2 dialer
/// itself rather than by us. Hardcoding `tun1` unconditionally — which an
/// early version of the bash did — made *every* strongswan-engine container
/// report unhealthy regardless of real tunnel state; found by live testing
/// (specs/012-strongswan-epdg).
pub fn tunnel_iface_for(line: &LineResolutionEntry, tunnel_engine: &str) -> String {
    if tunnel_engine == "strongswan" {
        line.strongswan_tun_iface.clone()
    } else {
        "tun1".to_string()
    }
}

/// `ip addr show <iface>` inside `netns` reports at least one inet/inet6
/// address. Mirrors the bash `grep -qE 'inet6? '`.
fn iface_has_address(runner: &dyn CommandRunner, netns: &str, iface: &str) -> bool {
    let Ok(out) = runner.run_in_netns(netns, &["ip", "addr", "show", iface]) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8_lossy(&out.stdout).lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("inet ") || t.starts_with("inet6 ")
    })
}

/// Evaluates one line's health. Pure decision logic over the runner seam —
/// no hardware, no root, no namespaces needed to test it.
pub fn check_line(
    runner: &dyn CommandRunner,
    line: &LineResolutionEntry,
    tunnel_engine: &str,
) -> Option<LineFault> {
    let iface = tunnel_iface_for(line, tunnel_engine);
    if !iface_has_address(runner, &line.netns, &iface) {
        return Some(LineFault::TunnelInterfaceHasNoAddress { iface });
    }

    // An absent or empty capture file means the tunnel has not reported a
    // P-CSCF yet. That is not itself a fault — the address check above
    // already passed, and the bash version likewise only probed when the
    // file was non-empty (`[ -s "$pcscf_path" ]`).
    let Ok(contents) = runner.read_file(std::path::Path::new(&line.pcscf_source_path)) else {
        return None;
    };
    let addr = contents.trim();
    if addr.is_empty() {
        return None;
    }
    // Probed from inside this line's namespace, never the default one. The
    // P-CSCF sits at the far end of this line's ePDG tunnel and has no route
    // to it outside `netns`, so a default-namespace probe reports every
    // healthy line as unreachable — which is precisely what this container's
    // HEALTHCHECK did, marking working deployments unhealthy while both lines
    // were registered and carrying calls.
    if !runner.tcp_connect_ok_in_netns(&line.netns, addr, PCSCF_SIP_PORT) {
        return Some(LineFault::PcscfUnreachable {
            addr: addr.to_string(),
        });
    }
    None
}

/// `gsm-sip-bridge tcp-probe <host> <port>` — exit 0 iff the connect succeeds.
///
/// Exists so [`CommandRunner::tcp_connect_ok_in_netns`] can perform a probe
/// inside a network namespace by re-executing this binary under
/// `ip netns exec`, rather than calling `setns` and introducing the only
/// `unsafe` block in the application binary.
pub fn run_tcp_probe(host: &str, port: u16) -> ExitCode {
    let runner = RealCommandRunner::new();
    if runner.tcp_connect_ok(host, port) {
        ExitCode::SUCCESS
    } else {
        eprintln!("tcp-probe: {host}:{port} unreachable");
        ExitCode::FAILURE
    }
}

/// The full probe, given an already-loaded resolution.
pub fn evaluate(
    runner: &dyn CommandRunner,
    metrics_ok: bool,
    cs_enabled: bool,
    vowifi_enabled: bool,
    resolution: &LineResolution,
    tunnel_engine: &str,
) -> Health {
    if !metrics_ok {
        return Health::MetricsEndpointDown;
    }
    // VoWiFi off entirely: nothing but the circuit-switched side to report
    // on. If that side is itself deliberately off
    // (specs/026-disable-circuit-switched), say so distinctly rather than
    // reporting a bare, uninformative "Healthy".
    if !vowifi_enabled {
        return if cs_enabled {
            Health::Healthy
        } else {
            Health::CircuitSwitchedDisabled
        };
    }

    // specs/027-discover-retry-health FR-008: a configured line
    // (`modem_port`/`modem_serial`) that never resolved at all must flip
    // this unhealthy too — checked *before* the old "no lines resolved,
    // nothing to report" shortcut below, which used to treat that exact
    // situation as healthy because it only ever looked at `resolution.lines`.
    // `max_lines_exceeded` is excluded: that is an unpinned auto-discovered
    // candidate losing out on a scarce slot, not a configured line failing.
    let configured_not_found: Vec<String> = resolution
        .failed
        .iter()
        .filter(|f| f.reason == "not_found")
        .map(|f| f.card_id.clone())
        .collect();

    if resolution.lines.is_empty() && configured_not_found.is_empty() {
        return if cs_enabled {
            Health::Healthy
        } else {
            Health::CircuitSwitchedDisabled
        };
    }

    let line_faults: Vec<(u32, LineFault)> = resolution
        .lines
        .iter()
        .filter_map(|l| check_line(runner, l, tunnel_engine).map(|f| (l.index, f)))
        .collect();

    if line_faults.is_empty() && configured_not_found.is_empty() {
        Health::Healthy
    } else {
        Health::LinesUnhealthy {
            line_faults,
            configured_not_found,
        }
    }
}

/// Whether the circuit-switched daemon's metrics endpoint is answering.
fn metrics_endpoint_ok(runner: &dyn CommandRunner, port: u16) -> bool {
    runner.tcp_connect_ok("127.0.0.1", port)
}

pub fn run(cli: &Cli) -> ExitCode {
    let Some(path) = cli.config.as_deref() else {
        eprintln!("healthcheck: --config is required");
        return ExitCode::FAILURE;
    };
    let config = match load_config(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("healthcheck: {e}");
            return ExitCode::FAILURE;
        }
    };

    let runner = RealCommandRunner::new();
    let metrics_ok = metrics_endpoint_ok(&runner, config.metrics.port);

    // Read what `supervise` resolved at startup; never re-scan (see the
    // module doc). A missing/unparsable file is an empty resolution, which
    // degrades to "circuit-switched only" rather than failing.
    let resolution = crate::vowifi::discovery::read_line_resolution(
        &crate::modules::discovery::lines_file_path(),
    )
    .unwrap_or_default();

    let health = evaluate(
        &runner,
        metrics_ok,
        config.cs.enabled,
        config.vowifi.enabled,
        &resolution,
        &config.vowifi.tunnel_engine,
    );

    match &health {
        Health::Healthy => ExitCode::SUCCESS,
        Health::CircuitSwitchedDisabled => ExitCode::SUCCESS,
        Health::MetricsEndpointDown => {
            eprintln!(
                "metrics endpoint on port {} is not responding",
                config.metrics.port
            );
            ExitCode::FAILURE
        }
        Health::LinesUnhealthy {
            line_faults,
            configured_not_found,
        } => {
            for (index, fault) in line_faults {
                eprintln!("line {index}: {fault}");
            }
            for identifier in configured_not_found {
                eprintln!("configured line {identifier}: never discovered");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;

    fn line(index: u32) -> LineResolutionEntry {
        LineResolutionEntry {
            index,
            card_id: format!("ec20-{index}"),
            modem_port: format!("/dev/ttyUSB{index}"),
            netns: format!("ims{index}"),
            control_port: 7100 + index as u16,
            veth_local_addr: format!("10.9.{index}.1/30"),
            veth_peer_addr: format!("10.9.{index}.2/30"),
            vpcd_port: 15963 + index as u16,
            strongswan_if_id: 42 + index,
            strongswan_tun_iface: format!("ipsec-{index}"),
            pcscf_source_path: "/tmp/pcscf".to_string(),
            mcc: "404".to_string(),
            mnc: "043".to_string(),
            pcsc_reader: false,
            config: Default::default(),
        }
    }

    fn resolution(lines: Vec<LineResolutionEntry>) -> LineResolution {
        LineResolution {
            circuit_switched_excluded_ports: vec![],
            lines,
            failed: vec![],
        }
    }

    /// Makes `ip addr show <iface>` in `netns` report an address.
    fn with_addressed_iface(runner: &MockCommandRunner, netns: &str, iface: &str) {
        runner.set_netns_output(
            netns,
            &["ip", "addr", "show", iface],
            "5: ipsec-0: <POINTOPOINT,UP> mtu 1400\n    inet 10.20.30.40/32 scope global ipsec-0\n",
        );
    }

    #[test]
    fn a_dead_metrics_endpoint_is_unhealthy_and_short_circuits_every_line_check() {
        let runner = MockCommandRunner::new();

        let health = evaluate(
            &runner,
            false,
            true,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );

        assert_eq!(health, Health::MetricsEndpointDown);
        // Nothing else was probed — no namespace commands were issued at all.
        assert!(runner.run_in_netns_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn vowifi_disabled_is_healthy_on_the_metrics_endpoint_alone() {
        let runner = MockCommandRunner::new();
        let health = evaluate(
            &runner,
            true,
            true,
            false,
            &resolution(vec![line(0)]),
            "strongswan",
        );
        assert_eq!(health, Health::Healthy);
    }

    /// The spec's degrade clarification: zero usable lines is a *reported*
    /// condition, not a container-failing one.
    #[test]
    fn zero_resolved_lines_is_healthy_not_a_container_failure() {
        let runner = MockCommandRunner::new();
        let health = evaluate(&runner, true, true, true, &resolution(vec![]), "strongswan");
        assert_eq!(health, Health::Healthy);
    }

    /// specs/027-discover-retry-health FR-008: this is the exact incident
    /// the feature exists for — a configured line never resolved at all
    /// (zero `lines`), which the old `resolution.lines.is_empty()`
    /// shortcut reported as bare `Healthy` even though the operator's
    /// config said a line should exist.
    #[test]
    fn a_configured_line_that_was_never_found_is_unhealthy_even_with_zero_resolved_lines() {
        let runner = MockCommandRunner::new();
        let mut r = resolution(vec![]);
        r.failed = vec![crate::vowifi::discovery::FailedLine::new(
            "/dev/ttyUSB3",
            "not_found",
        )];
        let health = evaluate(&runner, true, true, true, &r, "strongswan");
        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![],
                configured_not_found: vec!["/dev/ttyUSB3".to_string()],
            }
        );
    }

    /// A healthy resolved line does not mask a *sibling* configured line
    /// that never resolved — both must be visible in the same verdict.
    #[test]
    fn a_configured_line_not_found_is_unhealthy_alongside_an_otherwise_healthy_resolved_line() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "10.11.12.13\n");
        runner.set_tcp_connect_ok_in_netns("ims0", "10.11.12.13", 5060, true);

        let mut r = resolution(vec![line(0)]);
        r.failed = vec![crate::vowifi::discovery::FailedLine::new(
            "/dev/ttyUSB5",
            "not_found",
        )];
        let health = evaluate(&runner, true, true, true, &r, "strongswan");
        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![],
                configured_not_found: vec!["/dev/ttyUSB5".to_string()],
            }
        );
    }

    /// `max_lines_exceeded` is a different condition (an unpinned
    /// auto-discovered candidate losing out on a scarce slot) — must not be
    /// reported as a configured line failing to be discovered.
    #[test]
    fn max_lines_exceeded_does_not_count_as_a_configured_line_not_found() {
        let runner = MockCommandRunner::new();
        let mut r = resolution(vec![]);
        r.failed = vec![crate::vowifi::discovery::FailedLine::new(
            "ec20-AAAAAA",
            "max_lines_exceeded",
        )];
        let health = evaluate(&runner, true, true, true, &r, "strongswan");
        assert_eq!(health, Health::Healthy);
    }

    /// specs/026-disable-circuit-switched FR-018: [cs].enabled is false and
    /// there is nothing else to check — reported distinctly, not as a bare
    /// (uninformative) Healthy, and still not a container failure.
    #[test]
    fn cs_disabled_with_nothing_else_to_check_is_reported_distinctly() {
        let runner = MockCommandRunner::new();
        let health = evaluate(
            &runner,
            true,
            false,
            false,
            &resolution(vec![]),
            "strongswan",
        );
        assert_eq!(health, Health::CircuitSwitchedDisabled);
        assert!(health.is_healthy(), "must not be unhealthy/degraded/failed");
    }

    /// FR-018: with VoWiFi actually carrying traffic, [cs].enabled being
    /// false must not suppress the real per-line health checks — the
    /// distinct "disabled" verdict only applies when there is nothing else
    /// to report on.
    #[test]
    fn cs_disabled_does_not_suppress_real_vowifi_line_checks() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "10.11.12.13\n");
        runner.set_tcp_connect_ok_in_netns("ims0", "10.11.12.13", 5060, true);

        let health = evaluate(
            &runner,
            true,
            false,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );
        assert_eq!(health, Health::Healthy);
    }

    #[test]
    fn a_line_whose_tunnel_iface_has_no_address_is_unhealthy() {
        let runner = MockCommandRunner::new();
        // No `ip addr show` output seeded -> no address.
        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );

        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![(
                    0,
                    LineFault::TunnelInterfaceHasNoAddress {
                        iface: "ipsec-0".to_string()
                    }
                )],
                configured_not_found: vec![],
            }
        );
    }

    #[test]
    fn a_line_with_an_address_and_a_reachable_pcscf_is_healthy() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "10.11.12.13\n");
        runner.set_tcp_connect_ok_in_netns("ims0", "10.11.12.13", 5060, true);

        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );

        assert_eq!(health, Health::Healthy);
    }

    #[test]
    fn a_line_whose_pcscf_does_not_answer_sip_is_unhealthy() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "10.11.12.13\n");
        // No set_tcp_connect_ok -> connect fails.

        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );

        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![(
                    0,
                    LineFault::PcscfUnreachable {
                        addr: "10.11.12.13".to_string()
                    }
                )],
                configured_not_found: vec![],
            }
        );
    }

    /// No P-CSCF captured yet is not a fault — the tunnel is up, the address
    /// simply has not been reported. Matches the bash `[ -s "$pcscf_path" ]`.
    #[test]
    fn a_line_with_no_captured_pcscf_yet_is_not_faulted_for_it() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "  \n");

        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );

        assert_eq!(health, Health::Healthy);
    }

    /// specs/013-multi-card-vowifi FR-019: *every* line is checked, not only
    /// the first, and each failure is reported against its own index.
    #[test]
    fn every_line_is_checked_and_faults_are_reported_per_line() {
        let runner = MockCommandRunner::new();
        // Line 0 healthy, line 1 has no address, line 2 has an unreachable P-CSCF.
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        with_addressed_iface(&runner, "ims2", "ipsec-2");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "10.11.12.13\n");
        runner.set_tcp_connect_ok_in_netns("ims2", "10.11.12.13", 5060, false);

        let mut l0 = line(0);
        l0.pcscf_source_path = "/tmp/pcscf-0".to_string();
        runner.set_file(std::path::Path::new("/tmp/pcscf-0"), "10.0.0.1\n");
        runner.set_tcp_connect_ok_in_netns("ims0", "10.0.0.1", 5060, true);

        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![l0, line(1), line(2)]),
            "strongswan",
        );

        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![
                    (
                        1,
                        LineFault::TunnelInterfaceHasNoAddress {
                            iface: "ipsec-1".to_string()
                        }
                    ),
                    (
                        2,
                        LineFault::PcscfUnreachable {
                            addr: "10.11.12.13".to_string()
                        }
                    ),
                ],
                configured_not_found: vec![],
            }
        );
    }

    /// Regression test for a live-found bug: the P-CSCF was probed from the
    /// *default* namespace. It only has a route inside this line's own ePDG
    /// tunnel namespace, so the probe could never succeed — every genuinely
    /// healthy deployment reported unhealthy, while both lines were registered
    /// with their carriers and able to carry calls.
    ///
    /// Seeding *only* the default-namespace probe must therefore leave the
    /// line faulted: if this test passes with `Healthy`, the probe has
    /// regressed to the wrong namespace.
    #[test]
    fn the_pcscf_is_probed_inside_the_lines_namespace_not_the_default_one() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        runner.set_file(std::path::Path::new("/tmp/pcscf"), "10.11.12.13\n");
        // Reachable from the default namespace, and *only* from there.
        runner.set_tcp_connect_ok("10.11.12.13", 5060, true);

        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![line(0)]),
            "strongswan",
        );

        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![(
                    0,
                    LineFault::PcscfUnreachable {
                        addr: "10.11.12.13".to_string()
                    }
                )],
                configured_not_found: vec![],
            },
            "a default-namespace probe must not satisfy this check"
        );

        // ...and seeding it inside the line's namespace does.
        runner.set_tcp_connect_ok_in_netns("ims0", "10.11.12.13", 5060, true);
        assert_eq!(
            evaluate(
                &runner,
                true,
                true,
                true,
                &resolution(vec![line(0)]),
                "strongswan"
            ),
            Health::Healthy
        );
    }

    /// Each line is probed in its *own* namespace: one line's reachable P-CSCF
    /// must never vouch for another's, which sharing a namespace argument (or
    /// dropping it) would silently allow.
    #[test]
    fn one_lines_reachable_pcscf_does_not_vouch_for_another_line() {
        let runner = MockCommandRunner::new();
        with_addressed_iface(&runner, "ims0", "ipsec-0");
        with_addressed_iface(&runner, "ims1", "ipsec-1");

        let mut l0 = line(0);
        l0.pcscf_source_path = "/tmp/pcscf-0".to_string();
        let mut l1 = line(1);
        l1.pcscf_source_path = "/tmp/pcscf-1".to_string();
        // Both lines happen to have been assigned the same P-CSCF address by
        // their carriers — plausible, and the case where an unscoped probe is
        // most obviously wrong.
        runner.set_file(std::path::Path::new("/tmp/pcscf-0"), "10.11.12.13\n");
        runner.set_file(std::path::Path::new("/tmp/pcscf-1"), "10.11.12.13\n");
        runner.set_tcp_connect_ok_in_netns("ims0", "10.11.12.13", 5060, true);
        // ims1 deliberately unseeded -> unreachable through line 1's tunnel.

        let health = evaluate(
            &runner,
            true,
            true,
            true,
            &resolution(vec![l0, l1]),
            "strongswan",
        );

        assert_eq!(
            health,
            Health::LinesUnhealthy {
                line_faults: vec![(
                    1,
                    LineFault::PcscfUnreachable {
                        addr: "10.11.12.13".to_string()
                    }
                )],
                configured_not_found: vec![],
            }
        );
    }

    /// The live-found bug this encodes: hardcoding `tun1` for every engine
    /// made every strongswan-engine container report unhealthy regardless of
    /// real tunnel state.
    #[test]
    fn the_tunnel_iface_is_engine_specific() {
        let l = line(0);
        assert_eq!(tunnel_iface_for(&l, "strongswan"), "ipsec-0");
        assert_eq!(tunnel_iface_for(&l, "swu"), "tun1");
    }
}
