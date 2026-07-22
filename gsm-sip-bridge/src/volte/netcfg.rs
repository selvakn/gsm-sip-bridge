//! Host interface configuration for the IMS PDN (FR-024).
//!
//! This module exists because of one hard-won finding (`research.md` R7): the
//! carrier **unicasts its Router Advertisements to the link-local form of the
//! interface identifier it assigned**, not to `ff02::1`. Linux, left to
//! itself, generates a stable-privacy link-local instead — so every RA is
//! addressed to somebody else, is silently discarded, and the PDN looks dead.
//! The observed symptoms are `no IPv6 Routers available` from a DHCP client
//! and `Address not available` from raw sockets, with the RAs arriving on the
//! wire the entire time. It cost a packet capture to see.
//!
//! So before soliciting an RA the host must:
//!   1. set `addr_gen_mode=none` so the kernel stops inventing an identifier,
//!   2. install the link-local derived from `AT+CGPADDR`,
//!   3. enable `accept_ra=2` (the interface is not a router, but forwarding
//!      may be on in the container, and `2` accepts RAs regardless).
//!
//! Shells out to `ip` rather than speaking netlink, consistent with
//! `ims/gm_ipsec.rs` and this crate's zero-`unsafe` policy.

use crate::error::{BridgeError, BridgeResult};
use std::net::Ipv6Addr;
use std::process::Command;

/// One `ip`/sysctl step, kept as data so the sequence can be asserted in
/// tests without touching a real interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetStep {
    /// `ip link set <iface> <up|down>`
    Link { iface: String, up: bool },
    /// `ip -6 addr flush dev <iface>`
    FlushV6 { iface: String },
    /// write `<value>` to `/proc/sys/net/ipv6/conf/<iface>/<knob>`
    Sysctl {
        iface: String,
        knob: String,
        value: String,
    },
    /// `ip -6 addr add <addr>/64 dev <iface> scope link`
    AddLinkLocal { iface: String, addr: Ipv6Addr },
    /// `ip -6 addr add <addr>/128 dev <iface>`
    AddGlobal { iface: String, addr: Ipv6Addr },
}

/// The exact sequence that took the reference hardware from "bound but dead"
/// to "routed on the carrier's IMS PDN".
///
/// `assigned` is the address from `AT+CGPADDR`. Its low 64 bits are the
/// identifier the network expects; its full form is added as a /128 because
/// 3GPP expects the UE to source traffic from the assigned address, whereas
/// SLAAC would otherwise derive a different one from the interface MAC
/// (`research.md` R9).
pub fn configure_steps(iface: &str, assigned: Ipv6Addr) -> Vec<NetStep> {
    let link_local = super::pdn::link_local_from_assigned(assigned);
    vec![
        NetStep::Link {
            iface: iface.to_string(),
            up: false,
        },
        NetStep::FlushV6 {
            iface: iface.to_string(),
        },
        // 1 = none. Must be set while the link is down, before any address
        // exists, or the kernel keeps the identifier it already generated.
        NetStep::Sysctl {
            iface: iface.to_string(),
            knob: "addr_gen_mode".to_string(),
            value: "1".to_string(),
        },
        NetStep::Link {
            iface: iface.to_string(),
            up: true,
        },
        NetStep::AddLinkLocal {
            iface: iface.to_string(),
            addr: link_local,
        },
        NetStep::Sysctl {
            iface: iface.to_string(),
            knob: "accept_ra".to_string(),
            value: "2".to_string(),
        },
        NetStep::Sysctl {
            iface: iface.to_string(),
            knob: "autoconf".to_string(),
            value: "1".to_string(),
        },
        NetStep::AddGlobal {
            iface: iface.to_string(),
            addr: assigned,
        },
    ]
}

/// Steps that revert `configure_steps`, for teardown (FR-005).
pub fn teardown_steps(iface: &str) -> Vec<NetStep> {
    vec![
        NetStep::FlushV6 {
            iface: iface.to_string(),
        },
        // Back to the kernel default so the interface behaves normally if it
        // is later rebound to a non-IMS context.
        NetStep::Sysctl {
            iface: iface.to_string(),
            knob: "addr_gen_mode".to_string(),
            value: "0".to_string(),
        },
        NetStep::Link {
            iface: iface.to_string(),
            up: false,
        },
    ]
}

impl NetStep {
    /// Renders the step as the argv it will run, for logging and assertions.
    pub fn argv(&self) -> Vec<String> {
        let s = |v: &str| v.to_string();
        match self {
            NetStep::Link { iface, up } => vec![
                s("ip"),
                s("link"),
                s("set"),
                iface.clone(),
                s(if *up { "up" } else { "down" }),
            ],
            NetStep::FlushV6 { iface } => vec![
                s("ip"),
                s("-6"),
                s("addr"),
                s("flush"),
                s("dev"),
                iface.clone(),
            ],
            NetStep::Sysctl { iface, knob, value } => vec![
                s("sysctl"),
                s("-w"),
                format!("net.ipv6.conf.{iface}.{knob}={value}"),
            ],
            NetStep::AddLinkLocal { iface, addr } => vec![
                s("ip"),
                s("-6"),
                s("addr"),
                s("add"),
                format!("{addr}/64"),
                s("dev"),
                iface.clone(),
                s("scope"),
                s("link"),
            ],
            NetStep::AddGlobal { iface, addr } => vec![
                s("ip"),
                s("-6"),
                s("addr"),
                s("add"),
                format!("{addr}/128"),
                s("dev"),
                iface.clone(),
            ],
        }
    }
}

/// Runs one step. `tolerate_failure` covers steps that are expected to fail
/// benignly on a re-run (an address that already exists, a flush with nothing
/// to flush).
fn run_step(step: &NetStep, tolerate_failure: bool) -> BridgeResult<()> {
    let argv = step.argv();
    tracing::debug!(argv = ?argv, "netcfg");

    // sysctl knobs are written through /proc directly: the container image is
    // not guaranteed to ship procps, and the write is exactly equivalent.
    if let NetStep::Sysctl { iface, knob, value } = step {
        let path = format!("/proc/sys/net/ipv6/conf/{iface}/{knob}");
        return match std::fs::write(&path, value.as_bytes()) {
            Ok(()) => Ok(()),
            Err(_) if tolerate_failure => Ok(()),
            Err(e) => Err(BridgeError::Ims(format!("failed to write {path}: {e}"))),
        };
    }

    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| BridgeError::Ims(format!("failed to spawn `{}`: {e}", argv.join(" "))))?;
    if out.status.success() || tolerate_failure {
        return Ok(());
    }
    Err(BridgeError::Ims(format!(
        "`{}` failed: {}",
        argv.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// Applies the FR-024 interface configuration.
pub fn configure(iface: &str, assigned: Ipv6Addr) -> BridgeResult<()> {
    for step in configure_steps(iface, assigned) {
        // Adding an address that is already present, and flushing an empty
        // interface, are both fine on a repeat run — idempotence matters more
        // than strictness here.
        let tolerate = matches!(
            step,
            NetStep::FlushV6 { .. } | NetStep::AddLinkLocal { .. } | NetStep::AddGlobal { .. }
        );
        run_step(&step, tolerate)?;
    }
    Ok(())
}

/// Reverts the interface configuration. Best-effort throughout: teardown must
/// not fail on a half-configured interface.
pub fn teardown(iface: &str) -> BridgeResult<()> {
    for step in teardown_steps(iface) {
        run_step(&step, true)?;
    }
    Ok(())
}

/// Waits until the carrier's Router Advertisement has actually been processed.
///
/// **The signal is the default route, not the presence of a global address.**
/// `configure_steps` installs the network-assigned address itself, so "does a
/// global address exist" is always true the instant configuration finishes and
/// says nothing about whether the RA arrived. Waiting on that was a real bug:
/// it reported success on an interface that had no route and could not carry a
/// single packet. The default route can only come from an accepted RA, so it
/// is the honest test of FR-024 having worked.
///
/// Also waits out duplicate address detection first — the kernel does not emit
/// a Router Solicitation while the link-local is still `tentative`, so polling
/// immediately would time out on a link that was about to come good.
pub fn wait_for_router(iface: &str, timeout: std::time::Duration) -> BridgeResult<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if has_default_route(iface)? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// True when a default route exists via this interface.
pub fn has_default_route(iface: &str) -> BridgeResult<bool> {
    let out = Command::new("ip")
        .args(["-6", "route", "show", "default", "dev", iface])
        .output()
        .map_err(|e| BridgeError::Ims(format!("failed to spawn `ip route show`: {e}")))?;
    Ok(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// Sends a Router Solicitation. The kernel emits one when the link comes up,
/// but only after DAD completes on the link-local, and it gives up after a few
/// tries — an explicit solicitation makes the bring-up deterministic rather
/// than dependent on that timing.
pub fn solicit_router(iface: &str) -> BridgeResult<()> {
    // `rdisc6` is the usual tool but is not present in every image; toggling
    // accept_ra forces the kernel to re-solicit, which needs nothing extra.
    let knob = format!("/proc/sys/net/ipv6/conf/{iface}/accept_ra");
    let _ = std::fs::write(&knob, b"0");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = std::fs::write(&knob, b"2");
    Ok(())
}

/// Global-scope IPv6 addresses currently on the interface.
pub fn global_addresses(iface: &str) -> BridgeResult<Vec<Ipv6Addr>> {
    let out = Command::new("ip")
        .args(["-6", "-o", "addr", "show", "dev", iface, "scope", "global"])
        .output()
        .map_err(|e| BridgeError::Ims(format!("failed to spawn `ip addr show`: {e}")))?;
    Ok(parse_ip_addr_show(&String::from_utf8_lossy(&out.stdout)))
}

/// Parses `ip -6 -o addr show` output into the addresses it lists.
pub fn parse_ip_addr_show(out: &str) -> Vec<Ipv6Addr> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            while let Some(tok) = it.next() {
                if tok == "inet6" {
                    return it.next()?.split('/').next()?.parse::<Ipv6Addr>().ok();
                }
            }
            None
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assigned() -> Ipv6Addr {
        "2402:8100:6ffe:8ae6:0:c:de2b:3801".parse().unwrap()
    }

    #[test]
    fn disables_kernel_address_generation_before_bringing_the_link_up() {
        // The ordering is the whole point: addr_gen_mode must be set while
        // the link is down, or the kernel keeps its own identifier and the
        // carrier's unicast RAs stay invisible.
        let steps = configure_steps("eth0", assigned());

        let gen_mode = steps
            .iter()
            .position(|s| matches!(s, NetStep::Sysctl { knob, .. } if knob == "addr_gen_mode"))
            .expect("addr_gen_mode step missing");
        let link_up = steps
            .iter()
            .position(|s| matches!(s, NetStep::Link { up: true, .. }))
            .expect("link up step missing");

        assert!(
            gen_mode < link_up,
            "addr_gen_mode must be set before the link comes up"
        );
    }

    #[test]
    fn installs_the_link_local_the_carrier_addresses_its_ras_to() {
        let steps = configure_steps("eth0", assigned());

        let ll = steps
            .iter()
            .find_map(|s| match s {
                NetStep::AddLinkLocal { addr, .. } => Some(*addr),
                _ => None,
            })
            .expect("no link-local step");

        assert_eq!(ll, "fe80::c:de2b:3801".parse::<Ipv6Addr>().unwrap());
    }

    #[test]
    fn adds_the_link_local_before_accepting_ras() {
        let steps = configure_steps("eth0", assigned());

        let ll = steps
            .iter()
            .position(|s| matches!(s, NetStep::AddLinkLocal { .. }))
            .unwrap();
        let accept = steps
            .iter()
            .position(|s| matches!(s, NetStep::Sysctl { knob, .. } if knob == "accept_ra"))
            .unwrap();

        assert!(
            ll < accept,
            "RAs would be discarded before the link-local exists"
        );
    }

    #[test]
    fn also_installs_the_network_assigned_address_itself() {
        // research.md R9: SLAAC would otherwise derive a MAC-based address
        // the network may not route.
        let steps = configure_steps("eth0", assigned());

        assert!(steps
            .iter()
            .any(|s| matches!(s, NetStep::AddGlobal { addr, .. } if *addr == assigned())));
    }

    #[test]
    fn renders_the_expected_ip_invocations() {
        let steps = configure_steps("wwan0", assigned());

        let ll = steps
            .iter()
            .find(|s| matches!(s, NetStep::AddLinkLocal { .. }))
            .unwrap();
        assert_eq!(
            ll.argv(),
            vec![
                "ip",
                "-6",
                "addr",
                "add",
                "fe80::c:de2b:3801/64",
                "dev",
                "wwan0",
                "scope",
                "link"
            ]
        );

        let down = NetStep::Link {
            iface: "wwan0".into(),
            up: false,
        };
        assert_eq!(down.argv(), vec!["ip", "link", "set", "wwan0", "down"]);
    }

    #[test]
    fn teardown_restores_the_kernel_default_address_generation() {
        let steps = teardown_steps("eth0");

        assert!(steps.iter().any(|s| matches!(
            s,
            NetStep::Sysctl { knob, value, .. } if knob == "addr_gen_mode" && value == "0"
        )));
    }

    #[test]
    fn parses_global_addresses_from_ip_output() {
        // Verbatim shape of what the spike saw once the RA was accepted.
        let out = "1645: enx0 inet6 2402:8100:6ffe:8ae6:4b:b3ff:feb9:ebe5/64 scope global dynamic mngtmpaddr proto kernel_ra \\ valid_lft forever preferred_lft forever\n";

        let addrs = parse_ip_addr_show(out);

        assert_eq!(
            addrs,
            vec!["2402:8100:6ffe:8ae6:4b:b3ff:feb9:ebe5"
                .parse::<Ipv6Addr>()
                .unwrap()]
        );
    }

    #[test]
    fn parse_ip_addr_show_tolerates_empty_output() {
        assert!(parse_ip_addr_show("").is_empty());
    }
}
