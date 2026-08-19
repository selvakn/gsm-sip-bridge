//! Per-line ePDG namespace/XFRM-interface setup (specs/021-entrypoint-supervise-rust
//! Phase 4) — originally a 1:1 port of `docker/entrypoint.sh`'s
//! `ensure_epdg_interface`. Idempotent: safe to call again on every
//! line-supervisor restart, exactly like the bash original.
//!
//! It has since grown two things the bash never had, both because a line's
//! XFRM `if_id` can stay claimed by a *previous* container run:
//! [`reclaim_stale_xfrm`], which releases those claims at startup when it can
//! prove every claim is ours, and a return value from
//! [`ensure_epdg_interface`] saying whether the interface actually ended up
//! present — without which a recreation that could never succeed retried
//! silently forever.

use super::runner::CommandRunner;
use std::collections::BTreeSet;

/// What to do about XFRM state found in the host's default namespace at
/// startup.
#[derive(Debug, PartialEq, Eq)]
pub enum XfrmReclaim {
    /// Nothing installed — no if_id is claimed, so nothing is in our way.
    Empty,
    /// Every entry belongs to this deployment (its if_id is one our own lines
    /// use), so it is leftover from a previous run of this container and safe
    /// to clear.
    AllOurs,
    /// Something here is not ours: an if_id outside our lines' range, or an
    /// entry carrying no if_id at all. Leave the host's IPsec alone.
    ForeignPresent,
}

/// Classifies a combined `ip xfrm state` + `ip xfrm policy` dump.
///
/// Errs toward leaving things alone: anything not positively identifiable as
/// ours — a foreign if_id, an entry with no if_id, a socket policy — makes the
/// whole dump `ForeignPresent`. Deleting selectively by if_id would avoid the
/// question entirely, but iproute2 has no such filter (`ip xfrm policy
/// deleteall if_id N` is rejected outright), so flushing everything is the
/// only tool available and "all ours or nothing" is the only safe rule to put
/// around it.
pub fn classify_xfrm_dump(dump: &str, ours: &BTreeSet<u32>) -> XfrmReclaim {
    let mut entries = 0usize;
    let mut ours_entries = 0usize;
    let mut foreign = false;

    for line in dump.lines() {
        // Both dumps start each entry at column 0. `socket` policies are
        // counted too: they carry no if_id, so they can only ever push this
        // toward `ForeignPresent`, which is the cautious direction.
        if line.starts_with("src ") || line.starts_with("socket ") {
            entries += 1;
        }
        if let Some(rest) = line.trim().strip_prefix("if_id ") {
            let raw = rest.split_whitespace().next().unwrap_or("");
            let parsed = raw
                .strip_prefix("0x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| raw.parse::<u32>().ok());
            match parsed {
                Some(id) if ours.contains(&id) => ours_entries += 1,
                _ => foreign = true,
            }
        }
    }

    if entries == 0 {
        return XfrmReclaim::Empty;
    }
    // Every entry must have accounted for itself with an if_id of ours.
    if foreign || ours_entries < entries {
        return XfrmReclaim::ForeignPresent;
    }
    XfrmReclaim::AllOurs
}

/// Clears XFRM state left behind by a previous run of this container, which
/// would otherwise keep our lines' `if_id`s claimed and make their tunnel
/// interfaces impossible to create.
///
/// Must run before charon starts, while none of our own tunnels exist — at
/// that point anything present is by definition stale.
///
/// Does nothing unless every entry is identifiably ours; see
/// [`classify_xfrm_dump`].
pub fn reclaim_stale_xfrm(runner: &dyn CommandRunner, ours: &BTreeSet<u32>) {
    let dump = |args: &[&str]| -> Option<String> {
        runner
            .run(args)
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };

    // A query that *failed* is not a query that returned nothing. Treating it
    // as an empty dump would let half an inventory authorize a flush of both
    // halves: if `ip xfrm policy` failed while `ip xfrm state` happened to hold
    // only our own if_ids, the guard below would see `AllOurs` and delete
    // policies it had never looked at — possibly an unrelated deployment's.
    // The flush is unfiltered, so it may only ever be authorized by a complete
    // picture (Greptile P1).
    let (Some(state), Some(policy)) = (
        dump(&["ip", "xfrm", "state"]),
        dump(&["ip", "xfrm", "policy"]),
    ) else {
        eprintln!(
            "[supervise] could not read the host's XFRM state, so it was left untouched. \
             Stale SAs/policies from a previous run can degrade a line's tunnel; clear \
             them by hand if one misbehaves — see docs/operations.md. (A line reporting \
             its if_id is already claimed is usually unrelated and self-clearing.)"
        );
        return;
    };
    let combined = format!("{state}\n{policy}");

    match classify_xfrm_dump(&combined, ours) {
        XfrmReclaim::Empty => {}
        XfrmReclaim::AllOurs => {
            println!(
                "[supervise] clearing XFRM state left by a previous run (it keeps this \
                 deployment's if_ids claimed, which makes the per-line tunnel interfaces \
                 impossible to create)"
            );
            let _ = runner.run(&["ip", "xfrm", "policy", "flush"]);
            let _ = runner.run(&["ip", "xfrm", "state", "flush"]);
        }
        XfrmReclaim::ForeignPresent => {
            eprintln!(
                "[supervise] found XFRM state that is not this deployment's, so it was left \
                 untouched. Clear it by hand if a line's tunnel misbehaves — see \
                 docs/operations.md. (A line reporting its if_id is already claimed is \
                 usually unrelated and self-clearing.)"
            );
        }
    }
}

/// Idempotently ensures netns `netns` and its pre-created XFRM interface
/// `tun_iface` (if_id `if_id`) exist, pinned per line since
/// specs/013-multi-card-vowifi replicates this recipe once per line rather
/// than sharing one namespace/interface across lines.
/// Returns whether `tun_iface` is actually present in `netns` when this
/// returns. Callers must report `false` rather than carrying on silently: the
/// recovery loop's whole job is to recreate a missing interface, and a
/// recreation that cannot succeed makes it spin forever.
/// The gateway the host would send `dest` through, from `ip route get` output.
///
/// `None` when the destination is on-link (no `via`) or the output cannot be
/// read — both meaning "no next hop to test", which callers must treat as
/// "cannot tell" rather than "broken".
///
/// Pure over the output so the parse is testable against real `ip` text.
pub fn next_hop_from_route_get(output: &str) -> Option<String> {
    // `49.44.190.250 via 192.168.100.1 dev eth0 src 192.168.100.2 uid 0`
    let mut tokens = output.split_whitespace();
    while let Some(tok) = tokens.next() {
        if tok == "via" {
            return tokens.next().map(str::to_string);
        }
    }
    None
}

/// Whether this host still has a usable path toward the ePDG.
///
/// Answers the question that has to be asked before tearing a tunnel down: is
/// the carrier unreachable because *this tunnel* is broken, or because this box
/// has no internet? On 2026-08-19 the answer was the latter — a scheduled router
/// reboot — and tearing the tunnel down over it converted a two-minute blip into
/// a six-hour outage.
///
/// The ePDG itself cannot be probed directly: measured against Jio's ePDG on
/// 2026-08-19, it drops ICMP entirely (100% loss) while the tunnel is perfectly
/// healthy, so pinging it would report "down" always and gate recovery off
/// permanently. What *is* testable is the next hop the host would use to reach
/// it, which is exactly what fails when the router reboots — and because the
/// ePDG is reached through that hop, a dead hop guarantees a dead ePDG path.
///
/// Returns `Err(next_hop)` only on positive evidence of a dead uplink: a next
/// hop was identified and did not answer. Anything ambiguous — no `via`, an
/// unreadable `ip route get`, a missing `ping` — returns `Ok(())`, because
/// failing to determine WAN health must not silently disable tunnel recovery.
pub fn epdg_path_ok(runner: &dyn CommandRunner, epdg_ip: &str) -> Result<(), String> {
    let Some(out) = runner.run(&["ip", "route", "get", epdg_ip]).ok() else {
        return Ok(());
    };
    if !out.status.success() {
        return Ok(());
    }
    let Some(next_hop) = next_hop_from_route_get(&String::from_utf8_lossy(&out.stdout)) else {
        return Ok(());
    };
    // One echo request, two-second deadline: this runs per line per tick and the
    // hop is on the local segment, so a healthy answer is sub-millisecond.
    let answered = runner
        .run(&["ping", "-c", "1", "-W", "2", &next_hop])
        .map(|o| o.status.success())
        .unwrap_or(true); // `ping` missing or unrunnable is "cannot tell".
    if answered {
        Ok(())
    } else {
        Err(next_hop)
    }
}

/// Whether this line's netns has a default route through its tunnel.
///
/// Worth asking on its own, because a line can have a perfectly healthy
/// CHILD_SA, a present interface and a carrier-assigned address while having no
/// route at all — and in that state every connect fails with `ENETUNREACH`
/// while every structural check passes. See [`ensure_default_route`].
pub fn has_default_route(runner: &dyn CommandRunner, netns: &str, tun_iface: &str) -> bool {
    runner
        .run_in_netns(netns, &["ip", "route", "show", "default", "dev", tun_iface])
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Install the default route through the tunnel, for both families.
///
/// Returns whether an IPv4 default route is present afterwards — the family
/// that carries Gm for the carriers here.
///
/// **This is not the only thing that installs it**, and deliberately so:
/// `docker/strongswan/ims.updown` installs it alongside the carrier address on
/// every `up-client`, because the kernel deletes an interface's default route
/// when the last address of that family is removed. That made the route a
/// casualty of every CHILD_SA teardown, including the ones this function's
/// callers perform themselves, and cost a 6-hour outage on 2026-08-19 when a
/// 2-minute WAN blip tore the SA down and the reconnect restored only the
/// address. Both places install it; both are idempotent.
///
/// Failures are logged rather than discarded. The previous version threw away
/// the result of every one of these commands, which is why 202 consecutive
/// recovery attempts produced no record of why the data path never returned.
pub fn ensure_default_route(runner: &dyn CommandRunner, netns: &str, tun_iface: &str) -> bool {
    for family in [None, Some("-6")] {
        let mut argv = vec!["ip"];
        argv.extend(family);
        argv.extend(["route", "replace", "default", "dev", tun_iface]);
        match runner.run_in_netns(netns, &argv) {
            Ok(out) if !out.status.success() => eprintln!(
                "[supervise] could not install the {} default route via {tun_iface} in netns \
                 {netns}: {}",
                if family.is_some() { "IPv6" } else { "IPv4" },
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => eprintln!(
                "[supervise] could not run `ip route replace default dev {tun_iface}` in netns \
                 {netns}: {e}"
            ),
            _ => {}
        }
    }
    has_default_route(runner, netns, tun_iface)
}

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
            //
            // This message used to name `ip xfrm state flush && ip xfrm policy
            // flush` as the remedy. It is not one, and measuring it live on
            // 2026-07-31 showed why: an XFRM interface registers its if_id in
            // the namespace it was *created* in (here the host's), not the one
            // it is moved to, so the previous run's `tun23-N` sitting in an
            // `imsN` namespace the kernel has not reaped yet holds the id with
            // no XFRM state involved at all. Nothing flushes a netdev. That
            // reap took ~2.5min every time, whatever the shutdown did —
            // `reclaim_stale_xfrm` above, the shutdown plan's `ip netns del`,
            // a clean exit 0, none of it made a difference. Waiting is the
            // whole remedy, so the message says so rather than sending the
            // operator after a leak that isn't there.
            match runner.run(&[
                "ip", "link", "add", tun_iface, "type", "xfrm", "if_id", if_id,
            ]) {
                Ok(out) if !out.status.success() => eprintln!(
                    "[supervise] could not create {tun_iface} (xfrm if_id {if_id}): {}. \
                     Usually this is the previous run of this container: its namespaces \
                     take a few minutes to be reaped and its interface holds the if_id \
                     until they are, which the kernel reports this way even though no \
                     interface of that name is visible anywhere. It clears itself and \
                     the line recovers on a later tick — flushing XFRM state/policy does \
                     not speed it up. See docs/operations.md.",
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
    ensure_default_route(runner, netns, tun_iface);
    // Received IPsec traffic gets dropped if IPsec policy isn't disabled on
    // the interface itself (osmocom wiki's Option 2 walkthrough).
    //
    // BOTH families need this, and for a long time only `ipv6` was set. A
    // carrier that hands out an IPv4 P-CSCF (Jio does; Airtel's v6 one is why
    // this went unnoticed) runs Gm IPsec over IPv4, and with `ipv4` left at 0
    // the kernel drops every inbound packet *after* it has already verified
    // the ICV and decrypted it. Nothing in /proc/net/xfrm_stat counts that
    // drop, so the only visible symptom was the protected-port connect timing
    // out — which reads exactly like the network never answering. Measured
    // 2026-08-14 against Jio: the P-CSCF's SYN-ACKs were arriving with valid
    // ICVs, correct TCP checksums and correct ACK numbers the whole time.
    // The per-interface knob alone is sufficient; conf/all stays untouched.
    for family in ["ipv4", "ipv6"] {
        let _ = runner.run_in_netns(
            netns,
            &[
                "sh",
                "-c",
                &format!("echo 1 > /proc/sys/net/{family}/conf/{tun_iface}/disable_policy"),
            ],
        );
    }

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

    #[test]
    fn next_hop_is_read_from_real_ip_route_get_output() {
        // Real shape from the affected Pi, where the ePDG is reached via the
        // router that reboots at 3AM.
        assert_eq!(
            next_hop_from_route_get(
                "49.44.190.250 via 192.168.100.1 dev eth0 src 192.168.100.2 uid 0 \n    cache \n"
            ),
            Some("192.168.100.1".to_string())
        );
    }

    #[test]
    fn an_on_link_destination_has_no_next_hop_to_test() {
        // No `via`: the destination is on the local segment. That is "cannot
        // tell", not "the uplink is down" -- returning a hop here would gate the
        // teardown on a probe of the ePDG itself, which (measured against Jio)
        // drops ICMP entirely even when the tunnel is perfectly healthy.
        assert_eq!(
            next_hop_from_route_get("10.125.208.240 dev wwan0 src 10.125.208.241 uid 0\n"),
            None
        );
        assert_eq!(next_hop_from_route_get(""), None);
        assert_eq!(next_hop_from_route_get("via"), None, "truncated output");
    }

    #[test]
    fn a_dead_next_hop_is_the_only_thing_reported_as_a_wan_outage() {
        // Positive evidence only. `ping` failing is a WAN outage; anything the
        // check cannot determine must read as healthy, because failing to
        // establish WAN health must never disable tunnel recovery.
        let runner = MockCommandRunner::new();
        runner.set_run_output(
            "ip route get 198.51.100.7",
            success_output("198.51.100.7 via 192.168.100.1 dev eth0 src 192.168.100.2 uid 0\n"),
        );
        runner.set_run_output("ping -c 1 -W 2 192.168.100.1", failure_output());
        assert_eq!(
            epdg_path_ok(&runner, "198.51.100.7"),
            Err("192.168.100.1".to_string())
        );

        // Hop answers: the uplink is fine, so an unreachable P-CSCF really is
        // the tunnel's problem.
        let runner = MockCommandRunner::new();
        runner.set_run_output(
            "ip route get 198.51.100.7",
            success_output("198.51.100.7 via 192.168.100.1 dev eth0 src 192.168.100.2 uid 0\n"),
        );
        assert_eq!(epdg_path_ok(&runner, "198.51.100.7"), Ok(()));

        // Route lookup itself fails: cannot tell.
        let runner = MockCommandRunner::new();
        runner.set_run_output("ip route get 198.51.100.7", failure_output());
        assert_eq!(epdg_path_ok(&runner, "198.51.100.7"), Ok(()));
    }

    fn ids(v: &[u32]) -> BTreeSet<u32> {
        v.iter().copied().collect()
    }

    // Real `ip xfrm` output shapes, trimmed to the parts this parses.
    const OURS_ONLY: &str = "\
src 192.168.15.10 dst 203.88.11.33
\tproto esp spi 0x57065f59 reqid 2 mode tunnel
\tif_id 0x18
src 2402:8100::1/128 dst ::/0 
\tdir out priority 334463 
\tif_id 0x17
";

    #[test]
    fn a_dump_that_is_entirely_ours_is_safe_to_clear() {
        assert_eq!(
            classify_xfrm_dump(OURS_ONLY, &ids(&[23, 24])),
            XfrmReclaim::AllOurs
        );
    }

    #[test]
    fn an_empty_dump_needs_no_action() {
        assert_eq!(classify_xfrm_dump("", &ids(&[23, 24])), XfrmReclaim::Empty);
    }

    #[test]
    fn a_foreign_if_id_protects_the_whole_dump() {
        // Someone else s IPsec on the same host. Flushing is all-or-nothing
        // (iproute2 cannot delete by if_id), so anything unrecognised must
        // stop us touching any of it.
        let dump = "src 10.0.0.1 dst 10.0.0.2\n\tif_id 0x99\n";
        assert_eq!(
            classify_xfrm_dump(dump, &ids(&[23, 24])),
            XfrmReclaim::ForeignPresent
        );
    }

    #[test]
    fn an_entry_carrying_no_if_id_at_all_also_protects_the_dump() {
        // Plain IPsec, unrelated to this project — it has no if_id to match,
        // so it must never be counted as ours by omission.
        let dump = "src 10.0.0.1 dst 10.0.0.2\n\tproto esp spi 0x1 reqid 1 mode tunnel\n";
        assert_eq!(
            classify_xfrm_dump(dump, &ids(&[23, 24])),
            XfrmReclaim::ForeignPresent
        );
    }

    #[test]
    fn a_socket_policy_protects_the_dump() {
        let dump = "socket in priority 0 \n\tdir in\n";
        assert_eq!(
            classify_xfrm_dump(dump, &ids(&[23, 24])),
            XfrmReclaim::ForeignPresent
        );
    }

    #[test]
    fn our_own_if_id_from_a_different_line_count_is_still_foreign() {
        // if_id 0x19 (25) would be line 2 — not configured in this run, so it
        // is not ours to delete.
        let dump = "src 10.0.0.1 dst 10.0.0.2\n\tif_id 0x19\n";
        assert_eq!(
            classify_xfrm_dump(dump, &ids(&[23, 24])),
            XfrmReclaim::ForeignPresent
        );
    }

    #[test]
    fn reclaim_flushes_only_when_everything_is_ours() {
        let runner = MockCommandRunner::new();
        runner.set_run_output("ip xfrm state", success_output(OURS_ONLY));
        runner.set_run_output("ip xfrm policy", success_output(""));
        reclaim_stale_xfrm(&runner, &ids(&[23, 24]));
        let calls = runner.run_calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c == &["ip", "xfrm", "policy", "flush"]));
        assert!(calls.iter().any(|c| c == &["ip", "xfrm", "state", "flush"]));
    }

    /// Regression test for Greptile P1: a query that *fails* must not be read
    /// as "nothing there". Half an inventory could otherwise authorize a flush
    /// of both halves — the state dump holding only our if_ids while the policy
    /// dump failed unseen, deleting policies belonging to a deployment we never
    /// looked at. The flush is unfiltered, so only a complete picture may
    /// authorize it.
    #[test]
    fn reclaim_never_flushes_on_a_half_failed_inventory() {
        for failed in ["ip xfrm state", "ip xfrm policy"] {
            let runner = MockCommandRunner::new();
            // The half that succeeds looks entirely like ours — the tempting case.
            runner.set_run_output("ip xfrm state", success_output(OURS_ONLY));
            runner.set_run_output("ip xfrm policy", success_output(OURS_ONLY));
            runner.set_run_output(failed, failure_output());

            reclaim_stale_xfrm(&runner, &ids(&[23, 24]));

            let calls = runner.run_calls.lock().unwrap();
            assert!(
                !calls.iter().any(|c| c.contains(&"flush".to_string())),
                "`{failed}` failing must veto the flush, not read as an empty dump"
            );
        }
    }

    #[test]
    fn reclaim_never_flushes_a_host_with_foreign_ipsec() {
        let runner = MockCommandRunner::new();
        runner.set_run_output(
            "ip xfrm state",
            success_output("src 10.0.0.1 dst 10.0.0.2\n\tif_id 0x99\n"),
        );
        runner.set_run_output("ip xfrm policy", success_output(""));
        reclaim_stale_xfrm(&runner, &ids(&[23, 24]));
        let calls = runner.run_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains(&"flush".to_string())),
            "someone else s IPsec must never be flushed"
        );
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
        //
        // Both families are asserted because for a long time only `ipv6` was
        // written, which silently broke Gm IPsec against every carrier with
        // an IPv4 P-CSCF: the kernel verified and decrypted the inbound
        // packet and then dropped it, uncounted. Assert v4 explicitly so the
        // family can never be dropped again.
        let runner = MockCommandRunner::new();
        ensure_epdg_interface(&runner, "ims0", "tun23-0", "23");
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        for family in ["ipv4", "ipv6"] {
            let expected = format!("echo 1 > /proc/sys/net/{family}/conf/tun23-0/disable_policy");
            assert!(
                netns_calls
                    .iter()
                    .any(|(ns, c)| ns == "ims0" && c == &["sh", "-c", expected.as_str()]),
                "missing disable_policy write for {family}"
            );
        }
    }

    fn failure_output() -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(256), // exit code 1
            stdout: vec![],
            stderr: vec![],
        }
    }

    fn success_output(stdout: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
        }
    }
}
