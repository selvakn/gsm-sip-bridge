//! IMS PDN lifecycle over AT (specs/015-volte-host-ims, US1).
//!
//! Establishes a PDP context on the carrier's IMS APN and binds it to the
//! host's network device, so the bridge's own IMS stack can send and receive
//! signalling over LTE instead of delegating to the modem's internal IMS
//! stack.
//!
//! The command sequence here is not guesswork — it is the transcript verified
//! against a live Vodafone India network on an EC200U (see
//! `specs/015-volte-host-ims/research.md` R1):
//!
//! ```text
//! AT+CGDCONT=<cid>,"IPV4V6","ims"   define the IMS context
//! AT+CGACT=1,<cid>                  activate it; the network grants the PDN
//! AT+CGCONTRDP=<cid>                read back the *assigned* APN + bearer id
//! AT+CGPADDR=<cid>                  read the assigned address
//! AT+QNETDEVCTL=1,<cid>,1           bind the host netdev to this context
//! ```
//!
//! Two hardware realities shape this module:
//!
//! 1. **The PDN is IPv6-only.** `AT+CGPADDR` reports `0.0.0.0` for IPv4 and a
//!    real IPv6 address. Treating a zero IPv4 address as failure would reject
//!    a perfectly good PDN.
//! 2. **`QNETDEVCTL` re-points the modem's single host-facing data path.**
//!    Binding the IMS context displaces whatever was bound before, so the
//!    previous binding is captured for restoration on teardown (FR-005/FR-006).

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::{AtCommander, AtResponse};
use std::net::{Ipv4Addr, Ipv6Addr};

/// An established IMS PDN, as the modem reports it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImsPdn {
    pub cid: u8,
    /// What we asked for, e.g. `ims`.
    pub apn_requested: String,
    /// What the network resolved it to, e.g. `ims.mnc043.mcc404.gprs`. Its
    /// presence is the evidence that the carrier really granted an IMS PDN
    /// rather than silently reusing the default bearer.
    pub apn_assigned: String,
    pub bearer_id: u8,
    pub ipv4: Option<Ipv4Addr>,
    pub ipv6: Option<Ipv6Addr>,
}

impl ImsPdn {
    /// Address family actually usable on this PDN, for operator reporting
    /// (FR-003).
    pub fn family(&self) -> &'static str {
        match (self.ipv4.is_some(), self.ipv6.is_some()) {
            (true, true) => "dual-stack",
            (false, true) => "IPv6-only",
            (true, false) => "IPv4-only",
            (false, false) => "none",
        }
    }
}

/// `2402:8100:6ffe:8ae6:0:c:de2b:3801` -> `fe80::c:de2b:3801`.
///
/// Keeps the low 64 bits (the interface identifier) and replaces the prefix
/// with the link-local one.
pub fn link_local_from_assigned(addr: Ipv6Addr) -> Ipv6Addr {
    let s = addr.segments();
    Ipv6Addr::new(0xfe80, 0, 0, 0, s[4], s[5], s[6], s[7])
}

/// Splits an AT response payload on commas that are outside double quotes.
///
/// `+CGCONTRDP` mixes bare integers with quoted strings, and the quoted
/// address fields are dot-separated (never comma-separated), so quote-aware
/// splitting is enough — no full CSV parser required.
pub fn split_at_fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// Converts the dot-decimal byte form 3GPP uses in `+CGCONTRDP` into an IPv6
/// address. The field carries 32 bytes (address then netmask) for IPv6, or 8
/// (address then mask) for IPv4; only the leading 16 bytes are the address.
pub fn dotted_to_ipv6(s: &str) -> Option<Ipv6Addr> {
    let bytes: Vec<u8> = s
        .split('.')
        .filter_map(|b| b.trim().parse::<u8>().ok())
        .collect();
    if bytes.len() < 16 {
        return None;
    }
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&bytes[..16]);
    Some(Ipv6Addr::from(octets))
}

/// Parsed `+CGCONTRDP` payload for one context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextParams {
    pub cid: u8,
    pub bearer_id: u8,
    pub apn_assigned: String,
}

/// Parses `+CGCONTRDP: <cid>,<bearer_id>,"<apn>",...` for the given cid.
///
/// The modem emits one line per address family, so the first match wins —
/// `cid`, `bearer_id`, and `apn` are identical across them.
pub fn parse_cgcontrdp(lines: &[String], cid: u8) -> Option<ContextParams> {
    for line in lines {
        let Some(payload) = line.trim().strip_prefix("+CGCONTRDP:") else {
            continue;
        };
        let f = split_at_fields(payload);
        if f.len() < 3 {
            continue;
        }
        let Ok(parsed_cid) = f[0].parse::<u8>() else {
            continue;
        };
        if parsed_cid != cid {
            continue;
        }
        let Ok(bearer_id) = f[1].parse::<u8>() else {
            continue;
        };
        return Some(ContextParams {
            cid: parsed_cid,
            bearer_id,
            apn_assigned: f[2].clone(),
        });
    }
    None
}

/// Parses `+CGPADDR: <cid>,"<v4>,<v6>"` into the addresses actually assigned.
///
/// An all-zero IPv4 address means "not assigned" and is reported as `None` —
/// this is the normal case on an IPv6-only IMS PDN, not an error.
pub fn parse_cgpaddr(lines: &[String], cid: u8) -> (Option<Ipv4Addr>, Option<Ipv6Addr>) {
    for line in lines {
        let Some(payload) = line.trim().strip_prefix("+CGPADDR:") else {
            continue;
        };
        let f = split_at_fields(payload);
        if f.len() < 2 || f[0].parse::<u8>() != Ok(cid) {
            continue;
        }
        let mut v4 = None;
        let mut v6 = None;
        // The addresses live in one quoted field, comma-separated; the
        // quote-aware splitter keeps them together, so split again here.
        for part in f[1..].iter().flat_map(|s| s.split(',')) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if let Ok(a) = part.parse::<Ipv4Addr>() {
                if !a.is_unspecified() {
                    v4 = Some(a);
                }
            } else if let Ok(a) = part.parse::<Ipv6Addr>() {
                if !a.is_unspecified() {
                    v6 = Some(a);
                }
            }
        }
        return (v4, v6);
    }
    (None, None)
}

/// Parses the `<stat>` field of `+CEREG: <n>,<stat>[,...]` — the EPS
/// registration state. `1` = registered home, `5` = registered roaming, `2` =
/// not registered but searching, `0` = not registered and not searching.
/// Returns `None` if no `+CEREG` line is present or it cannot be parsed.
pub fn parse_cereg_stat(lines: &[String]) -> Option<u8> {
    for line in lines {
        let Some(payload) = line.trim().strip_prefix("+CEREG:") else {
            continue;
        };
        let f = split_at_fields(payload);
        if f.len() < 2 {
            continue;
        }
        if let Ok(stat) = f[1].trim().parse::<u8>() {
            return Some(stat);
        }
    }
    None
}

/// Parses `+CGACT: <cid>,<state>` lines into (cid, active) pairs.
pub fn parse_cgact(lines: &[String]) -> Vec<(u8, bool)> {
    lines
        .iter()
        .filter_map(|l| {
            let payload = l.trim().strip_prefix("+CGACT:")?;
            let f = split_at_fields(payload);
            if f.len() < 2 {
                return None;
            }
            Some((f[0].parse::<u8>().ok()?, f[1].trim() == "1"))
        })
        .collect()
}

/// Current host-netdev binding, from `+QNETDEVCTL: <op>,<cid>,<urc>,<state>`.
///
/// Returns the bound cid, or `None` when nothing is bound (`0,0,0,0`).
pub fn parse_qnetdevctl(lines: &[String]) -> Option<u8> {
    for line in lines {
        let Some(payload) = line.trim().strip_prefix("+QNETDEVCTL:") else {
            continue;
        };
        let f = split_at_fields(payload);
        if f.len() < 2 {
            continue;
        }
        let cid = f[1].parse::<u8>().ok()?;
        return if cid == 0 { None } else { Some(cid) };
    }
    None
}

fn expect_ok(resp: AtResponse, what: &str) -> BridgeResult<Vec<String>> {
    match resp {
        AtResponse::Ok(lines) => Ok(lines),
        AtResponse::Error(e) => Err(BridgeError::Ims(format!("{what} failed: {e}"))),
        AtResponse::CmeError(code, msg) => Err(BridgeError::Ims(format!(
            "{what} failed: +CME ERROR: {code} ({msg})"
        ))),
    }
}

/// Which context, if any, currently owns the host-facing data path.
pub fn bound_context(at: &mut AtCommander) -> BridgeResult<Option<u8>> {
    let lines = expect_ok(at.send_command("AT+QNETDEVCTL?")?, "AT+QNETDEVCTL?")?;
    Ok(parse_qnetdevctl(&lines))
}

/// True when the given context is already active.
pub fn is_active(at: &mut AtCommander, cid: u8) -> BridgeResult<bool> {
    let lines = expect_ok(at.send_command("AT+CGACT?")?, "AT+CGACT?")?;
    Ok(parse_cgact(&lines)
        .into_iter()
        .any(|(c, active)| c == cid && active))
}

/// Reads back an already-active context. Returns `None` if the network never
/// assigned an APN, which means the PDN was not genuinely granted.
pub fn read_pdn(
    at: &mut AtCommander,
    cid: u8,
    apn_requested: &str,
) -> BridgeResult<Option<ImsPdn>> {
    let rdp = expect_ok(
        at.send_command(&format!("AT+CGCONTRDP={cid}"))?,
        "AT+CGCONTRDP",
    )?;
    let Some(params) = parse_cgcontrdp(&rdp, cid) else {
        return Ok(None);
    };
    if params.apn_assigned.is_empty() {
        return Ok(None);
    }
    let paddr = expect_ok(at.send_command(&format!("AT+CGPADDR={cid}"))?, "AT+CGPADDR")?;
    let (ipv4, ipv6) = parse_cgpaddr(&paddr, cid);
    Ok(Some(ImsPdn {
        cid,
        apn_requested: apn_requested.to_string(),
        apn_assigned: params.apn_assigned,
        bearer_id: params.bearer_id,
        ipv4,
        ipv6,
    }))
}

/// Activates the context, retrying transient refusals.
///
/// Observed on hardware: activating straight after a deactivate returns
/// `+CME ERROR: 3` (operation not allowed) while the modem is still settling,
/// and the identical command succeeds a second later. Failing the whole
/// attachment on that would make teardown-then-attach an intermittent error
/// for no reason.
fn activate_with_retry(at: &mut AtCommander, cid: u8) -> BridgeResult<()> {
    const ATTEMPTS: usize = 4;
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match at.send_command(&format!("AT+CGACT=1,{cid}"))? {
            AtResponse::Ok(_) => return Ok(()),
            AtResponse::Error(e) => last = e,
            AtResponse::CmeError(code, msg) => last = format!("+CME ERROR: {code} ({msg})"),
        }
        if attempt < ATTEMPTS {
            tracing::debug!(cid, attempt, error = %last, "CGACT refused; retrying");
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
    }
    Err(BridgeError::Ims(format!(
        "AT+CGACT failed after {ATTEMPTS} attempts: {last}"
    )))
}

/// Reads the PDN, waiting for the network to actually assign an address.
///
/// `AT+CGACT=1` returns `OK` as soon as the context is *active*, which is
/// before the address assignment lands. Reading `AT+CGPADDR` immediately
/// therefore races the network and intermittently reports a PDN with no
/// address at all — which then silently skips host interface configuration and
/// leaves an attachment that cannot carry traffic. Observed on live hardware;
/// it looks like an interface bug and is not one.
///
/// An active context that never produces an address is a real failure, so this
/// is bounded rather than infinite.
pub fn read_pdn_when_addressed(
    at: &mut AtCommander,
    cid: u8,
    apn: &str,
    timeout: std::time::Duration,
) -> BridgeResult<ImsPdn> {
    let deadline = std::time::Instant::now() + timeout;
    let mut last: Option<ImsPdn>;
    loop {
        match read_pdn(at, cid, apn)? {
            Some(pdn) if pdn.ipv4.is_some() || pdn.ipv6.is_some() => return Ok(pdn),
            other => last = other,
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    match last {
        Some(pdn) => Err(BridgeError::Ims(format!(
            "context {cid} (APN \"{}\") is active but the network assigned no address \
             within {timeout:?}; the PDN cannot carry traffic",
            pdn.apn_assigned
        ))),
        None => Err(BridgeError::Ims(format!(
            "the network did not grant an IMS PDN on cid {cid} (APN \"{apn}\"): \
             no assigned APN reported"
        ))),
    }
}

/// Outcome of bringing the PDN up, so callers can tell the operator whether
/// anything actually changed (US1 scenario 2) and what was displaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdnBringUp {
    pub pdn: ImsPdn,
    /// True when an already-active PDN was reused rather than created.
    pub reused: bool,
    /// The context the host data path was bound to beforehand, if any — what
    /// teardown restores (FR-005), and what FR-006 warns about.
    pub displaced_cid: Option<u8>,
}

/// Establishes the IMS PDN and binds it to the host data path.
///
/// Idempotent (FR-004): an already-active, already-bound context is reported
/// as reused rather than torn down and rebuilt.
pub fn bring_up(at: &mut AtCommander, cid: u8, apn: &str) -> BridgeResult<PdnBringUp> {
    let previously_bound = bound_context(at)?;
    let already_active = is_active(at, cid)?;

    if !already_active {
        expect_ok(
            at.send_command(&format!("AT+CGDCONT={cid},\"IPV4V6\",\"{apn}\""))?,
            "AT+CGDCONT",
        )?;
        activate_with_retry(at, cid)?;
    }

    let pdn = read_pdn_when_addressed(at, cid, apn, std::time::Duration::from_secs(15))?;

    if previously_bound != Some(cid) {
        expect_ok(
            at.send_command(&format!("AT+QNETDEVCTL=1,{cid},1"))?,
            "AT+QNETDEVCTL",
        )?;
    }

    Ok(PdnBringUp {
        pdn,
        reused: already_active && previously_bound == Some(cid),
        displaced_cid: previously_bound.filter(|c| *c != cid),
    })
}

/// Reads the current EPS registration state (`AT+CEREG?`), returning the
/// `<stat>` value for logging. Best-effort: a failure reads as `None` rather
/// than aborting whatever teardown step wanted to record it.
fn eps_registration_state(at: &mut AtCommander) -> Option<u8> {
    match at.send_command("AT+CEREG?") {
        Ok(AtResponse::Ok(lines)) => parse_cereg_stat(&lines),
        _ => None,
    }
}

/// Forces a packet-domain detach so the network releases the PDN — and with it
/// the PCRF's IP-CAN session — instead of leaving it orphaned.
///
/// **Why this is not optional** (`research.md` R19 / `hardening.md` H2, the
/// root cause of this feature's Rx 5065 block): deactivating the context with
/// `AT+CGACT=0` returns the *host* to "no PDN" but never detaches the UE from
/// the packet domain, so the core can keep a stale Gx session bound to the old
/// address. A later attach then creates a *new* IP-CAN session while the old
/// one still owns the binding, and every Rx `AAR` for media is answered
/// `IP-CAN_SESSION_NOT_AVAILABLE` (Rx 5065). Measured directly: after a clean
/// teardown the modem still reported `+CGATT: 1` / `+CEREG: 0,1` — attached —
/// which is why every "restart and it was identical" test was incapable of
/// clearing the session it was blaming.
///
/// `AT+CGATT=0` issues a real Detach Request: the PGW tears down every PDN
/// connection and sends the `CCR-T` that releases the session. RF is left on,
/// so the modem re-attaches to the packet domain on its own and the next
/// `bring_up` activates a clean context. On an exclusively-assigned card
/// (`research.md` R6) detaching every context is safe — nothing else is using
/// this modem, and the default context auto-re-attaches.
///
/// The `CEREG` state is read before and after solely so the log distinguishes
/// "we detached" from "we asked and the modem ignored it" after the fact.
/// Best-effort: a refused detach is logged loudly but does not fail teardown,
/// which is cleanup and must not get stuck.
pub fn force_packet_domain_detach(at: &mut AtCommander) {
    let before = eps_registration_state(at);
    match at.send_command("AT+CGATT=0") {
        Ok(AtResponse::Ok(_)) => {
            let after = eps_registration_state(at);
            tracing::info!(
                cereg_before = ?before,
                cereg_after = ?after,
                "forced a packet-domain detach so the network releases the IP-CAN session"
            );
        }
        other => {
            tracing::warn!(
                cereg_before = ?before,
                response = ?other,
                "AT+CGATT=0 was refused — the network may retain a stale IP-CAN \
                 session, which can surface as Rx 5065 on the next attach"
            );
        }
    }
}

/// Releases the IMS PDN and restores any previously displaced binding
/// (FR-005). Safe when nothing is attached.
pub fn tear_down(at: &mut AtCommander, cid: u8, restore_cid: Option<u8>) -> BridgeResult<()> {
    if bound_context(at)? == Some(cid) {
        match restore_cid {
            Some(prev) => {
                expect_ok(
                    at.send_command(&format!("AT+QNETDEVCTL=1,{prev},1"))?,
                    "AT+QNETDEVCTL restore",
                )?;
            }
            None => {
                // No prior binding to restore; leave the data path unbound.
                let _ = at.send_command(&format!("AT+QNETDEVCTL=0,{cid},1"))?;
            }
        }
    }
    if is_active(at, cid)? {
        expect_ok(at.send_command(&format!("AT+CGACT=0,{cid}"))?, "AT+CGACT=0")?;
    }
    // Deactivating the context is not enough — the UE stays attached and the
    // core can keep the IP-CAN session. Force a real detach so it is released.
    force_packet_domain_detach(at);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Transcripts below are verbatim from the live Vodafone India / EC200U
    // session recorded in research.md.

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_the_assigned_apn_and_bearer_id() {
        let l = lines(&[
            "+CGCONTRDP: 3,6,\"ims.mnc043.mcc404.gprs\",\"36.2.129.0.111.254.138.230.0.0.0.12.222.43.56.1.255.255.255.255.255.255.255.255.0.0.0.0.0.0.0.0\",\"254.128.0.0.0.0.0.0.0.0.0.0.0.0.0.1\",\"0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0\",\"0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0\"",
        ]);

        let p = parse_cgcontrdp(&l, 3).unwrap();

        assert_eq!(p.cid, 3);
        assert_eq!(p.bearer_id, 6);
        assert_eq!(p.apn_assigned, "ims.mnc043.mcc404.gprs");
    }

    #[test]
    fn ignores_cgcontrdp_lines_for_other_contexts() {
        let l = lines(&[
            "+CGCONTRDP: 1,5,\"www.mnc043.mcc404.gprs\",\"10.90.218.248.255.255.255.0\",\"10.90.218.1\"",
            "+CGCONTRDP: 3,6,\"ims.mnc043.mcc404.gprs\",\"36.2.129.0\"",
        ]);

        assert_eq!(parse_cgcontrdp(&l, 3).unwrap().bearer_id, 6);
        assert_eq!(parse_cgcontrdp(&l, 1).unwrap().bearer_id, 5);
        assert!(parse_cgcontrdp(&l, 7).is_none());
    }

    #[test]
    fn parses_the_eps_registration_state() {
        // Verbatim shapes from the live session. The stat field is what says
        // whether a detach actually took (0/2) or the UE is still attached (1).
        assert_eq!(parse_cereg_stat(&lines(&["+CEREG: 0,1"])), Some(1));
        assert_eq!(parse_cereg_stat(&lines(&["+CEREG: 0,2"])), Some(2));
        assert_eq!(parse_cereg_stat(&lines(&["+CEREG: 0,0"])), Some(0));
        // Tolerates the longer form with location/act fields, and junk.
        assert_eq!(
            parse_cereg_stat(&lines(&["+CEREG: 2,1,\"1A2B\",\"00C3D4E5\",7"])),
            Some(1)
        );
        assert_eq!(parse_cereg_stat(&lines(&["OK"])), None);
    }

    #[test]
    fn treats_an_ipv6_only_pdn_as_valid_not_as_failure() {
        // The IPv4 slot really is 0.0.0.0 on this carrier; rejecting that
        // would throw away a working PDN.
        let l = lines(&["+CGPADDR: 3,\"0.0.0.0,2402:8100:6FFE:8AE6:0:C:DE2B:3801\""]);

        let (v4, v6) = parse_cgpaddr(&l, 3);

        assert_eq!(v4, None, "all-zero IPv4 must read as unassigned");
        assert_eq!(
            v6,
            Some("2402:8100:6ffe:8ae6:0:c:de2b:3801".parse().unwrap())
        );
    }

    #[test]
    fn parses_a_dual_stack_cgpaddr() {
        let l = lines(&["+CGPADDR: 1,\"10.90.218.248,2402:8100::1\""]);

        let (v4, v6) = parse_cgpaddr(&l, 1);

        assert_eq!(v4, Some("10.90.218.248".parse().unwrap()));
        assert_eq!(v6, Some("2402:8100::1".parse().unwrap()));
    }

    #[test]
    fn derives_the_link_local_the_network_expects() {
        // research.md R7: the RA was unicast to fe80::c:de2b:3801 while the
        // assigned address was 2402:8100:6ffe:8ae6:0:c:de2b:3801.
        let assigned: Ipv6Addr = "2402:8100:6ffe:8ae6:0:c:de2b:3801".parse().unwrap();

        let ll = link_local_from_assigned(assigned);

        assert_eq!(ll, "fe80::c:de2b:3801".parse::<Ipv6Addr>().unwrap());
    }

    #[test]
    fn reports_the_family_operators_see() {
        let mut pdn = ImsPdn {
            cid: 3,
            apn_requested: "ims".into(),
            apn_assigned: "ims.mnc043.mcc404.gprs".into(),
            bearer_id: 6,
            ipv4: None,
            ipv6: Some("2402:8100::1".parse().unwrap()),
        };
        assert_eq!(pdn.family(), "IPv6-only");

        pdn.ipv4 = Some("10.0.0.1".parse().unwrap());
        assert_eq!(pdn.family(), "dual-stack");
    }

    #[test]
    fn parses_activation_state_per_context() {
        let l = lines(&["+CGACT: 1,0", "+CGACT: 2,0", "+CGACT: 3,1"]);

        let states = parse_cgact(&l);

        assert_eq!(states, vec![(1, false), (2, false), (3, true)]);
    }

    #[test]
    fn reads_the_bound_context_and_the_unbound_sentinel() {
        assert_eq!(parse_qnetdevctl(&lines(&["+QNETDEVCTL: 1,3,1,1"])), Some(3));
        // 0,0,0,0 is what the modem reports after a reboot — not "cid 0".
        assert_eq!(parse_qnetdevctl(&lines(&["+QNETDEVCTL: 0,0,0,0"])), None);
        assert_eq!(parse_qnetdevctl(&lines(&[])), None);
    }

    #[test]
    fn converts_the_3gpp_dotted_byte_form_to_ipv6() {
        // 36.2.58.128... == 2402:3a80:...
        let addr = dotted_to_ipv6("36.2.58.128.35.20.187.61.0.0.0.37.255.44.37.1.255.255.255.255.255.255.255.255.0.0.0.0.0.0.0.0").unwrap();

        assert_eq!(
            addr,
            "2402:3a80:2314:bb3d:0:25:ff2c:2501"
                .parse::<Ipv6Addr>()
                .unwrap()
        );
    }

    #[test]
    fn dotted_to_ipv6_rejects_a_short_field() {
        assert_eq!(dotted_to_ipv6("10.90.218.1"), None);
    }

    #[test]
    fn splits_fields_without_breaking_quoted_values() {
        let f = split_at_fields(" 3,6,\"ims.mnc043.mcc404.gprs\",\"1.2.3.4\"");

        assert_eq!(f, vec!["3", "6", "ims.mnc043.mcc404.gprs", "1.2.3.4"]);
    }
}
