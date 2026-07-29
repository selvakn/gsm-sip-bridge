//! Per-line ePDG namespace/XFRM-interface setup (specs/021-entrypoint-supervise-rust
//! Phase 4) — 1:1 port of `docker/entrypoint.sh`'s `ensure_epdg_interface`.
//! Idempotent: safe to call again on every line-supervisor restart, exactly
//! like the bash original.

use super::runner::CommandRunner;

/// Idempotently ensures netns `netns` and its pre-created XFRM interface
/// `tun_iface` (if_id `if_id`) exist, pinned per line since
/// specs/013-multi-card-vowifi replicates this recipe once per line rather
/// than sharing one namespace/interface across lines.
/// Returns whether `tun_iface` is actually present in `netns` when this
/// returns. Callers must report `false` rather than carrying on silently: the
/// recovery loop's whole job is to recreate a missing interface, and a
/// recreation that cannot succeed makes it spin forever.
pub fn ensure_epdg_interface(
    runner: &dyn CommandRunner,
    netns: &str,
    tun_iface: &str,
    if_id: &str,
) -> bool {
    let netns_marker = format!("/var/run/netns/{netns}");
    let netns_exists = runner
        .run(&["test", "-e", &netns_marker])
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !netns_exists {
        let _ = runner.run(&["ip", "netns", "add", netns]);
    }
    let _ = runner.run_in_netns(netns, &["ip", "link", "set", "lo", "up"]);

    let iface_in_netns = runner
        .run_in_netns(netns, &["ip", "link", "show", tun_iface])
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !iface_in_netns {
        let iface_in_default = runner
            .run(&["ip", "link", "show", tun_iface])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if iface_in_default {
            // Leftover in the default netns from a previous run that didn't
            // get moved — absorb rather than fail (idempotent startup).
            let _ = runner.run(&["ip", "link", "set", tun_iface, "netns", netns]);
        } else {
            // Report this failure rather than discarding it. An `if_id` can
            // stay claimed by leftover state from a *previous* container run —
            // XFRM state/policy and interfaces in namespaces that outlive the
            // container — and the kernel then answers `RTNETLINK answers: File
            // exists` for an interface name that exists nowhere. Swallowing
            // that left the steady-state loop detecting a missing interface,
            // "recreating" it, finding it missing again, and tearing down a
            // perfectly good CHILD_SA to retry — every 30s, forever, with
            // nothing in the log saying why. Diagnosed live 2026-07-29 only by
            // replaying this exact command by hand.
            match runner.run(&[
                "ip", "link", "add", tun_iface, "type", "xfrm", "if_id", if_id,
            ]) {
                Ok(out) if !out.status.success() => eprintln!(
                    "[supervise] could not create {tun_iface} (xfrm if_id {if_id}): {}. \
                     An if_id still claimed by a previous run does this even when no \
                     interface of that name exists anywhere; with no tunnel running, \
                     `ip xfrm state flush && ip xfrm policy flush` on the host releases it.",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => {
                    eprintln!("[supervise] could not run `ip link add {tun_iface}`: {e}")
                }
                _ => {}
            }
            let _ = runner.run(&["ip", "link", "set", tun_iface, "netns", netns]);
        }
    }

    let _ = runner.run_in_netns(netns, &["ip", "link", "set", tun_iface, "up"]);
    let _ = runner.run_in_netns(
        netns,
        &["ip", "route", "replace", "default", "dev", tun_iface],
    );
    let _ = runner.run_in_netns(
        netns,
        &["ip", "-6", "route", "replace", "default", "dev", tun_iface],
    );
    // Received IPsec traffic gets dropped if IPsec policy isn't disabled on
    // the interface itself (osmocom wiki's Option 2 walkthrough).
    let _ = runner.run_in_netns(
        netns,
        &[
            "sh",
            "-c",
            &format!("echo 1 > /proc/sys/net/ipv6/conf/{tun_iface}/disable_policy"),
        ],
    );

    // Confirm rather than assume. Every step above deliberately ignores its
    // own result (they are all idempotent and individually survivable), so
    // without a final check the one outcome that actually matters — is the
    // interface there? — was never established.
    runner
        .run_in_netns(netns, &["ip", "link", "show", tun_iface])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::runner::MockCommandRunner;

    // MOCK JUSTIFICATION (constitution Principle I): stands in for real
    // `ip`/netns/XFRM kernel operations — none available in CI. The command
    // sequence issued is real production code.

    // MockCommandRunner defaults every unseeded `run`/`run_in_netns` call to
    // a *successful* Output — so every "does X exist" probe this function
    // makes must be explicitly seeded to fail where a test wants to
    // exercise the "absent" branch; success (the default) reads as "exists".
    fn seed_absent(runner: &MockCommandRunner, key: &str) {
        runner.set_run_output(key, failure_output());
    }

    /// Regression test for a live failure that took an hour to diagnose
    /// because it was entirely silent: the interface could not be created (an
    /// `if_id` left claimed by a previous container run makes the kernel
    /// answer `File exists` for a name that exists nowhere), so steady-state
    /// tore down a healthy CHILD_SA every 30s to retry a recreation that could
    /// never succeed. Reporting the outcome is what makes that loop
    /// explicable.
    #[test]
    fn reports_false_when_the_interface_is_still_absent_afterwards() {
        let runner = MockCommandRunner::new();
        // Missing from the netns before *and* after the create/move attempt.
        seed_absent(&runner, "netns:ims1:ip link show tun23-1");
        seed_absent(&runner, "ip link show tun23-1");

        assert!(
            !ensure_epdg_interface(&runner, "ims1", "tun23-1", "24"),
            "an interface that never appeared must be reported, not assumed"
        );
    }

    #[test]
    fn reports_true_once_the_interface_is_present_in_the_netns() {
        let runner = MockCommandRunner::new();
        // Unseeded probes default to success, i.e. the interface is there.
        assert!(ensure_epdg_interface(&runner, "ims0", "tun23-0", "23"));
    }

    #[test]
    fn creates_the_netns_when_the_marker_file_is_absent() {
        let runner = MockCommandRunner::new();
        seed_absent(&runner, "test -e /var/run/netns/ims0");
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let calls = runner.run_calls.lock().unwrap();
        assert!(calls.iter().any(|c| c == &["ip", "netns", "add", "ims0"]));
    }

    #[test]
    fn reuses_the_netns_when_the_marker_file_is_already_present() {
        let runner = MockCommandRunner::new();
        // Default (unseeded) is success = "exists" — no seeding needed.
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let calls = runner.run_calls.lock().unwrap();
        assert!(!calls.iter().any(|c| c == &["ip", "netns", "add", "ims0"]));
    }

    #[test]
    fn creates_the_xfrm_interface_when_absent_from_both_namespaces() {
        let runner = MockCommandRunner::new();
        seed_absent(&runner, "netns:ims0:ip link show tun23-0");
        seed_absent(&runner, "ip link show tun23-0");
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let calls = runner.run_calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c == &["ip", "link", "add", "tun23-0", "type", "xfrm", "if_id", "23"]));
        assert!(calls
            .iter()
            .any(|c| c == &["ip", "link", "set", "tun23-0", "netns", "ims0"]));
    }

    #[test]
    fn reuses_the_xfrm_interface_when_already_present_in_the_netns() {
        let runner = MockCommandRunner::new();
        // Default (unseeded) is success = "already in netns" — no seeding.
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let calls = runner.run_calls.lock().unwrap();
        assert!(!calls.iter().any(
            |c| c.first().map(String::as_str) == Some("ip") && c.contains(&"xfrm".to_string())
        ));
    }

    #[test]
    fn absorbs_a_leftover_interface_in_the_default_netns_instead_of_recreating_it() {
        let runner = MockCommandRunner::new();
        seed_absent(&runner, "netns:ims0:ip link show tun23-0");
        // "ip link show tun23-0" (default netns) left at the default
        // (success) = present there, the leftover-from-a-prior-run case.
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let calls = runner.run_calls.lock().unwrap();
        assert!(!calls.iter().any(
            |c| c.first().map(String::as_str) == Some("ip") && c.contains(&"xfrm".to_string())
        ));
        assert!(calls
            .iter()
            .any(|c| c == &["ip", "link", "set", "tun23-0", "netns", "ims0"]));
    }

    #[test]
    fn always_brings_the_interface_up_and_installs_default_routes() {
        let runner = MockCommandRunner::new();
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(netns_calls
            .iter()
            .any(|(ns, c)| ns == "ims0" && c == &["ip", "link", "set", "tun23-0", "up"]));
        assert!(netns_calls
            .iter()
            .any(|(ns, c)| ns == "ims0"
                && c == &["ip", "route", "replace", "default", "dev", "tun23-0"]));
        assert!(netns_calls.iter().any(|(ns, c)| ns == "ims0"
            && c == &["ip", "-6", "route", "replace", "default", "dev", "tun23-0"]));
    }

    #[test]
    fn disables_ipsec_policy_on_the_interface_so_received_traffic_is_not_dropped() {
        // Regression test for an FR-009 gap (T049 audit): the sysctl write
        // was ported (osmocom wiki's Option 2 walkthrough — received IPsec
        // traffic gets dropped if IPsec policy isn't disabled on the
        // interface itself) but nothing asserted it was actually issued.
        let runner = MockCommandRunner::new();
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(netns_calls.iter().any(|(ns, c)| ns == "ims0"
            && c == &[
                "sh",
                "-c",
                "echo 1 > /proc/sys/net/ipv6/conf/tun23-0/disable_policy"
            ]));
    }

    fn failure_output() -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(256), // exit code 1
            stdout: vec![],
            stderr: vec![],
        }
    }
}
