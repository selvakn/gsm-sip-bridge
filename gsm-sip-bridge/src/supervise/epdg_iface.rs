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
    match classify_and_maybe_flush(runner, ours) {
        XfrmFlushOutcome::Empty => {}
        XfrmFlushOutcome::Flushed => println!(
            "[supervise] clearing XFRM state left by a previous run (it keeps this \
             deployment's if_ids claimed, which makes the per-line tunnel interfaces \
             impossible to create)"
        ),
        XfrmFlushOutcome::LeftForeign => eprintln!(
            "[supervise] found XFRM state that is not this deployment's, so it was left \
             untouched. Clear it by hand if a line's tunnel misbehaves — see \
             docs/operations.md. (A line reporting its if_id is already claimed is \
             usually unrelated and self-clearing.)"
        ),
        XfrmFlushOutcome::Unreadable => eprintln!(
            "[supervise] could not read the host's XFRM state, so it was left untouched. \
             Stale SAs/policies from a previous run can degrade a line's tunnel; clear \
             them by hand if one misbehaves — see docs/operations.md. (A line reporting \
             its if_id is already claimed is usually unrelated and self-clearing.)"
        ),
    }
}

/// What [`classify_and_maybe_flush`] did, so each caller — startup's
/// [`reclaim_stale_xfrm`] and `shutdown`'s `FlushXfrm` step — can report it in
/// its own context-appropriate wording rather than sharing one message meant
/// for the other's caller.
#[derive(Debug, PartialEq, Eq)]
pub enum XfrmFlushOutcome {
    /// Nothing was there — no action, nothing to report.
    Empty,
    /// Everything present was ours; it has been flushed.
    Flushed,
    /// Something present was not identifiably ours; left untouched.
    LeftForeign,
    /// The inventory itself could not be read; left untouched (see
    /// [`reclaim_stale_xfrm`]'s doc comment on why a failed query is never
    /// treated as an empty one).
    Unreadable,
}

/// Reads the host's XFRM state and policy, classifies it via
/// [`classify_xfrm_dump`], and flushes it if and only if every entry is
/// identifiably ours (`ours`) — the one piece of logic startup's stale-claim
/// reclamation and stop's tunnel teardown must share exactly, since the flush
/// itself is unfiltered (iproute2 has no `ip xfrm policy deleteall if_id N`)
/// and the all-ours-or-nothing rule is the only safe guard around it.
pub fn classify_and_maybe_flush(
    runner: &dyn CommandRunner,
    ours: &BTreeSet<u32>,
) -> XfrmFlushOutcome {
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
        return XfrmFlushOutcome::Unreadable;
    };
    let combined = format!("{state}\n{policy}");

    match classify_xfrm_dump(&combined, ours) {
        XfrmReclaim::Empty => XfrmFlushOutcome::Empty,
        XfrmReclaim::AllOurs => {
            let _ = runner.run(&["ip", "xfrm", "policy", "flush"]);
            let _ = runner.run(&["ip", "xfrm", "state", "flush"]);
            XfrmFlushOutcome::Flushed
        }
        XfrmReclaim::ForeignPresent => XfrmFlushOutcome::LeftForeign,
    }
}

/// One namespace this deployment might have left behind from a previous,
/// ungraceful run — everything [`reclaim_leftover_lines`] needs to release it.
///
/// specs/041-shutdown-resource-cleanup US2/FR-014. Bearer-agnostic: built
/// from `LineResolutionEntry` for VoWiFi and from the VoLTE line manifest for
/// VoLTE, both in `orchestrate`/`orchestrate_volte`.
pub struct ReclaimCandidate {
    pub netns: String,
    /// `Some` for a strongswan-engine VoWiFi line only — the swu engine has
    /// no XFRM device, and VoLTE lines route through their assigned modem
    /// interface, not a tunnel device this run created.
    pub tun_iface: Option<String>,
    /// This line's host-side veth end, present for both bearers — `None`
    /// for a VoLTE line on the diagnostic single-`--modem` path, which
    /// never had one.
    pub veth_host: Option<String>,
    /// A device this deployment would have created **inside** `netns`, used
    /// as positive proof of ownership before anything is deleted. `None`
    /// means "cannot prove it", which vetoes reclaiming that namespace
    /// entirely — see [`reclaim_leftover_lines`].
    pub owned_iface_marker: Option<String>,
}

/// How long a single reclaim delete may run before it is abandoned — same
/// bound as the stop path's `DeleteLink` step (`shutdown::
/// DELETE_LINK_TIMEOUT_SECS`), kept as its own constant here rather than
/// imported so this module has no reason to depend on `shutdown`'s internals.
const RECLAIM_DELETE_TIMEOUT_SECS: u32 = 5;

/// Whether the operator has opted this deployment in to reclaiming leftover
/// namespaces (`GSM_SIP_BRIDGE_RECLAIM_LEFTOVER_NETNS=1`).
///
/// **Off by default, deliberately** (Greptile P1). Reclamation deletes
/// devices and namespaces on the *host*, through the `/var/run/netns` bind
/// mount this feature adds — a destructive capability the container simply
/// did not have before, because it could not see host namespaces at all.
/// Two things can own a name like `ims0`, and existence alone distinguishes
/// neither: an unrelated host workload (closed by the ownership marker
/// below), and a **concurrently running second instance of this
/// deployment** — which no check available from inside our own PID namespace
/// can rule out (`ip netns pids` cannot see another container's processes,
/// and `flock` needs `unsafe`, which `make lint` forbids — research.md R7).
///
/// R7 argues a second instance cannot work anyway (fixed namespace names,
/// fixed `if_id`s, charon wildcard-binding UDP 500/4500). That argument is
/// about the second instance *failing to start*; it says nothing about it
/// destroying the first one's live networking on the way, which is what this
/// code would do. That asymmetry — a plausible operator slip costing a live
/// line rather than a failed start — is why the destructive half is opt-in
/// and the graceful-stop half (this feature's proven 167s→8s result) is not.
pub fn reclaim_leftover_enabled() -> bool {
    std::env::var("GSM_SIP_BRIDGE_RECLAIM_LEFTOVER_NETNS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Releases every leftover namespace in `candidates` that already existed on
/// the host *before this call* — which, since this always runs before any of
/// this run's own line setup, means "not created by this run" needs no
/// separate check (FR-014, FR-016). A namespace not on the host is silently
/// skipped (SC-008: a clean host pays nothing for this).
///
/// Reclaims a namespace only when **all** of these hold, and reports rather
/// than acts otherwise:
///
/// 1. `enabled` — the operator opted in; see [`reclaim_leftover_enabled`].
/// 2. The namespace exists on the host.
/// 3. `owned_iface_marker` names a device that is actually **present inside
///    that namespace**. Existence of a *name* is not ownership (Greptile
///    P1): anything on the host could be called `ims0`, but only this
///    deployment puts a `tun23-N` inside one. This mirrors the
///    all-ours-or-nothing rule [`classify_xfrm_dump`] already applies to the
///    unfiltered XFRM flush — positive identification required, anything
///    unproven vetoes the destructive action.
///
/// Deliberately **not** the stop path's `TerminateIke` step: this run's own
/// charon has not started yet, so there is no live IKE_SA to ask it to
/// terminate through — only the abandoned device and namespace remain to
/// give back. XFRM state is a separate concern, already handled once per
/// caller by [`reclaim_stale_xfrm`] before this runs; that call cannot
/// release a netdev either way (research.md R1), which is the entire reason
/// this function exists.
pub fn reclaim_leftover_lines(
    runner: &dyn CommandRunner,
    candidates: &[ReclaimCandidate],
    enabled: bool,
) {
    for c in candidates {
        let exists = runner
            .run(&["test", "-e", &format!("/var/run/netns/{}", c.netns)])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !exists {
            continue;
        }

        // From here on the namespace is present, so every path must either
        // act deliberately or say why it did not — a silent skip here reads
        // as "clean host" and hides the reason a line is about to fail to
        // come up.
        let Some(marker) = &c.owned_iface_marker else {
            eprintln!(
                "[supervise] netns {} exists but this deployment has no device it could \
                 identify as its own inside it, so it was left untouched. If a line's \
                 tunnel cannot be created, clear it by hand — see docs/operations.md.",
                c.netns
            );
            continue;
        };
        let ours = runner
            .run_in_netns(&c.netns, &["ip", "link", "show", marker])
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ours {
            eprintln!(
                "[supervise] netns {} exists but does not contain {marker}, so it is not \
                 this deployment's and was left untouched. Something else on this host \
                 is using that name — see docs/operations.md.",
                c.netns
            );
            continue;
        }
        if !enabled {
            eprintln!(
                "[supervise] netns {} looks like a leftover from a previous run of this \
                 deployment ({marker} is present), but reclaiming it is opt-in and not \
                 enabled. If no second instance of this deployment is running, set \
                 GSM_SIP_BRIDGE_RECLAIM_LEFTOVER_NETNS=1 to have it cleared \
                 automatically — see docs/operations.md.",
                c.netns
            );
            continue;
        }
        println!(
            "[supervise] reclaiming netns {} left by a previous run of this deployment \
             ({marker} present)",
            c.netns
        );
        let ts = RECLAIM_DELETE_TIMEOUT_SECS.to_string();
        if let Some(tun) = &c.tun_iface {
            match runner.run_in_netns(&c.netns, &["timeout", &ts, "ip", "link", "del", tun]) {
                Ok(out) if !out.status.success() => eprintln!(
                    "[supervise] could not reclaim {tun} in netns {}: {}",
                    c.netns,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => {
                    eprintln!(
                        "[supervise] could not reclaim {tun} in netns {}: {e}",
                        c.netns
                    )
                }
                _ => {}
            }
        }
        if let Some(veth) = &c.veth_host {
            match runner.run(&["timeout", &ts, "ip", "link", "del", veth]) {
                Ok(out) if !out.status.success() => eprintln!(
                    "[supervise] could not reclaim {veth}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                Err(e) => eprintln!("[supervise] could not reclaim {veth}: {e}"),
                _ => {}
            }
        }
        match runner.run(&["ip", "netns", "del", &c.netns]) {
            Ok(out) if !out.status.success() => eprintln!(
                "[supervise] could not delete leftover netns {}: {}",
                c.netns,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => eprintln!(
                "[supervise] could not delete leftover netns {}: {e}",
                c.netns
            ),
            _ => {}
        }
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
            //
            // This message used to name `ip xfrm state flush && ip xfrm policy
            // flush` as the remedy. It is not one, and measuring it live on
            // 2026-07-31 showed why: an XFRM interface registers its if_id in
            // the namespace it was *created* in (here the host's), not the one
            // it is moved to, so the previous run's `tun23-N` sitting in an
            // `imsN` namespace the kernel has not reaped yet holds the id with
            // no XFRM state involved at all. Nothing flushes a netdev.
            //
            // specs/041-shutdown-resource-cleanup: at that time nothing gave
            // the device back either — the shutdown plan only removed the
            // *name* (`ip netns del`), never the device — so the ~2.5min
            // reap this message used to promise as "the whole remedy" was
            // really just the previous container's leftover mount namespace
            // finally expiring on its own. Two things now exist that did
            // not then: a graceful stop deletes the device explicitly
            // (`supervise::shutdown`'s `DeleteLink` step) before it ever
            // gets this far, and a fresh start reclaims a still-present
            // leftover from an *ungraceful* exit
            // (`reclaim_leftover_lines`, called above this function).
            // Reaching this message at all now means neither of those ran —
            // most plausibly the previous run is still mid-teardown (wait
            // for it) or was killed on a host where the namespace directory
            // isn't shared with the container (docker-compose.yml's
            // `/var/run/netns` bind mount, research.md R6) — see
            // docs/operations.md for what to check.
            match runner.run(&[
                "ip", "link", "add", tun_iface, "type", "xfrm", "if_id", if_id,
            ]) {
                Ok(out) if !out.status.success() => eprintln!(
                    "[supervise] could not create {tun_iface} (xfrm if_id {if_id}): {}. \
                     This if_id is claimed by an interface that still exists somewhere — \
                     normally either the previous run's stop is still in progress, or it \
                     was force-killed on a host where the namespace directory is not \
                     shared with this container. See docs/operations.md.",
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

    // Regression tests for specs/041-shutdown-resource-cleanup US2/FR-014:
    // reclaiming a previous, ungraceful run's leftover namespace/devices.

    /// A strongswan-line candidate that can prove ownership of `ims0`.
    fn provable_candidate() -> ReclaimCandidate {
        ReclaimCandidate {
            netns: "ims0".to_string(),
            tun_iface: Some("tun23-0".to_string()),
            veth_host: Some("veth-sip0".to_string()),
            owned_iface_marker: Some("tun23-0".to_string()),
        }
    }

    #[test]
    fn a_clean_host_with_no_leftover_namespace_triggers_no_delete_at_all() {
        // SC-008: a clean host must pay nothing beyond the existence check
        // itself for this reclamation to run every startup.
        let runner = MockCommandRunner::new();
        seed_absent(&runner, "test -e /var/run/netns/ims0");
        reclaim_leftover_lines(&runner, &[provable_candidate()], true);
        let calls = runner.run_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains(&"del".to_string())),
            "nothing should be deleted when the namespace was never there"
        );
    }

    #[test]
    fn a_leftover_namespace_has_its_tun_veth_and_netns_all_reclaimed() {
        let runner = MockCommandRunner::new();
        // Unseeded probes default to success: namespace exists, and the
        // ownership marker is present inside it.
        reclaim_leftover_lines(&runner, &[provable_candidate()], true);

        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(netns_calls.iter().any(|(ns, c)| ns == "ims0"
            && c.contains(&"del".to_string())
            && c.contains(&"tun23-0".to_string())));

        let calls = runner.run_calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.contains(&"del".to_string()) && c.contains(&"veth-sip0".to_string())));
        assert!(calls.iter().any(|c| c == &["ip", "netns", "del", "ims0"]));
    }

    #[test]
    fn a_volte_candidate_reclaims_via_its_in_namespace_veth_marker() {
        let runner = MockCommandRunner::new();
        reclaim_leftover_lines(
            &runner,
            &[ReclaimCandidate {
                netns: "volte0".to_string(),
                tun_iface: None,
                veth_host: Some("veth-tel0".to_string()),
                owned_iface_marker: Some("veth-carrier0".to_string()),
            }],
            true,
        );
        let calls = runner.run_calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.contains(&"del".to_string()) && c.contains(&"veth-tel0".to_string())));
        assert!(calls.iter().any(|c| c == &["ip", "netns", "del", "volte0"]));
        // No tun to delete, so no in-namespace delete is issued at all --
        // the only in-namespace call is the ownership probe itself.
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(!netns_calls
            .iter()
            .any(|(_, c)| c.contains(&"del".to_string())));
    }

    // --- Greptile P1: existence of a name is not ownership -----------------

    #[test]
    fn a_namespace_not_containing_our_marker_device_is_never_touched() {
        // Someone else's `ims0` on the same host. Deleting its links and
        // namespace would take out an unrelated workload's networking -- a
        // capability this feature's /var/run/netns bind mount newly makes
        // possible, so it must be positively guarded, not merely unlikely.
        let runner = MockCommandRunner::new();
        seed_absent(&runner, "netns:ims0:ip link show tun23-0");
        reclaim_leftover_lines(&runner, &[provable_candidate()], true);

        let calls = runner.run_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains(&"del".to_string())),
            "a namespace we cannot prove is ours must never be deleted"
        );
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(!netns_calls
            .iter()
            .any(|(_, c)| c.contains(&"del".to_string())));
    }

    #[test]
    fn a_candidate_that_cannot_prove_ownership_at_all_is_never_reclaimed() {
        // No marker device exists for this line (e.g. a swu-engine line, or
        // a VoLTE line with no veth) -> nothing can identify the namespace
        // as ours, so it is left alone rather than deleted on the strength
        // of its name.
        let runner = MockCommandRunner::new();
        reclaim_leftover_lines(
            &runner,
            &[ReclaimCandidate {
                netns: "volte0".to_string(),
                tun_iface: None,
                veth_host: None,
                owned_iface_marker: None,
            }],
            true,
        );
        let calls = runner.run_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains(&"del".to_string())),
            "an unprovable candidate must veto its own reclamation"
        );
    }

    #[test]
    fn reclamation_does_nothing_at_all_unless_explicitly_enabled() {
        // Default-off: the destructive path stays inert even when the
        // namespace is present AND provably ours, because nothing available
        // from inside our own PID namespace can rule out a concurrently
        // running second instance of this same deployment.
        let runner = MockCommandRunner::new();
        reclaim_leftover_lines(&runner, &[provable_candidate()], false);

        let calls = runner.run_calls.lock().unwrap();
        assert!(
            !calls.iter().any(|c| c.contains(&"del".to_string())),
            "opt-out by default: nothing may be deleted without the operator opting in"
        );
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(!netns_calls
            .iter()
            .any(|(_, c)| c.contains(&"del".to_string())));
    }

    #[test]
    fn every_delete_step_is_bounded() {
        let runner = MockCommandRunner::new();
        reclaim_leftover_lines(&runner, &[provable_candidate()], true);
        let calls = runner.run_calls.lock().unwrap();
        assert!(calls
            .iter()
            .any(|c| c.contains(&"veth-sip0".to_string()) && c[0] == "timeout"));
        let netns_calls = runner.run_in_netns_calls.lock().unwrap();
        assert!(netns_calls
            .iter()
            .any(|(_, c)| c.contains(&"del".to_string())
                && c.contains(&"tun23-0".to_string())
                && c[0] == "timeout"));
    }
}
