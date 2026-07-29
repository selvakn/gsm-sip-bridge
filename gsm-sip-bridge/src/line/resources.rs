//! Per-line resource derivation: turning a line's index into the isolated
//! namespace, interface names, addresses, and ports that make it independent
//! of every other line.
//!
//! Nothing here is configurable. A line's index is the only input, which is
//! what makes the whole scheme collision-free by construction rather than by
//! an operator getting a table of port numbers right.

/// Adds `delta` to a dotted-quad address.
///
/// Used to step whole `/30` veth blocks between lines. Existed twice,
/// byte-identical, in `vowifi::discovery` and `volte::discovery` before this
/// module.
pub fn shift_ipv4(addr: &str, delta: u32) -> Option<String> {
    let ip: std::net::Ipv4Addr = addr.parse().ok()?;
    let shifted = u32::from(ip).checked_add(delta)?;
    Some(std::net::Ipv4Addr::from(shifted).to_string())
}

/// One `/30` veth block per line — stepping the whole dotted-quad by 4 keeps
/// each line's pair inside its own subnet with no overlap.
pub const VETH_BLOCK_STRIDE: u32 = 4;

/// The address offset for line `index`.
pub fn veth_offset(index: u32) -> u32 {
    index * VETH_BLOCK_STRIDE
}

/// A per-line name derived from a base and an index: `"ims"` + `0` → `"ims0"`.
///
/// **Uniform for every line, including index 0.** VoWiFi always did this;
/// VoLTE special-cased index 0 to keep its unindexed base name (`"volte"`
/// rather than `"volte0"`) for backwards compatibility with the
/// single-line deployments that predated multi-line support. That left two
/// subsystems with two different rules for the same derivation — and made
/// line 0 the one line whose names could not be predicted from its index,
/// which is exactly the line most likely to exist.
///
/// The special case is gone. Every line's namespace, interface, and address
/// names are now a pure function of its index in both subsystems.
pub fn indexed(base: &str, index: u32) -> String {
    format!("{base}{index}")
}

/// A port derived from a base and a per-line stride, for subsystems that give
/// each line a contiguous block of loopback ports.
pub fn strided_port(base: u16, index: u32, stride: u16) -> u16 {
    base.saturating_add((index as u16).saturating_mul(stride))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_names_are_uniform_including_line_zero() {
        // The regression this encodes: VoLTE used to return the bare base
        // for index 0, so `volte`/`volte1`/`volte2` rather than
        // `volte0`/`volte1`/`volte2`.
        assert_eq!(indexed("ims", 0), "ims0");
        assert_eq!(indexed("ims", 1), "ims1");
        assert_eq!(indexed("volte", 0), "volte0");
        assert_eq!(indexed("volte", 3), "volte3");
    }

    #[test]
    fn veth_blocks_do_not_overlap_between_adjacent_lines() {
        // Line N's pair must not land inside line N+1's /30.
        let base = "10.98.0.1";
        let l0 = shift_ipv4(base, veth_offset(0)).unwrap();
        let l1 = shift_ipv4(base, veth_offset(1)).unwrap();
        let l2 = shift_ipv4(base, veth_offset(2)).unwrap();
        assert_eq!(l0, "10.98.0.1");
        assert_eq!(l1, "10.98.0.5");
        assert_eq!(l2, "10.98.0.9");
    }

    #[test]
    fn shift_ipv4_carries_across_octet_boundaries() {
        assert_eq!(shift_ipv4("10.0.0.254", 4).unwrap(), "10.0.1.2");
    }

    #[test]
    fn shift_ipv4_rejects_a_non_address_rather_than_guessing() {
        assert_eq!(shift_ipv4("not-an-ip", 4), None);
        assert_eq!(shift_ipv4("", 4), None);
        // IPv6 is not a dotted quad — this scheme is v4-only by construction.
        assert_eq!(shift_ipv4("fe80::1", 4), None);
    }

    #[test]
    fn shift_ipv4_saturates_rather_than_wrapping_at_the_top_of_the_space() {
        assert_eq!(shift_ipv4("255.255.255.255", 1), None);
    }

    #[test]
    fn strided_ports_give_each_line_its_own_block() {
        assert_eq!(strided_port(6000, 0, 4), 6000);
        assert_eq!(strided_port(6000, 1, 4), 6004);
        assert_eq!(strided_port(6000, 2, 4), 6008);
    }

    /// A port derivation that wrapped would silently collide two lines onto
    /// one socket; saturating turns an absurd index into an obviously wrong
    /// port instead.
    #[test]
    fn strided_ports_saturate_rather_than_wrapping() {
        assert_eq!(strided_port(65000, 1000, 4), u16::MAX);
    }
}
