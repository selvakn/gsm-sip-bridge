//! P-CSCF discovery probes (specs/015-volte-host-ims, US2).
//!
//! # These probes are diagnostics, not the supported way to get an address
//!
//! Gate G1 established that the tested carrier (Vodafone India) publishes the
//! P-CSCF by **no** mechanism reachable from the host. Every probe here is
//! expected to come back empty on that network; the configured override is
//! what actually works today. They are still worth running and reporting,
//! because a different carrier, SIM, or firmware may behave differently and
//! this is what makes that discoverable without a code change (US2 scenario 5).
//!
//! So "success" for this module is a *complete and accurate report*, not a
//! discovered address — which is exactly what SC-002 was amended to say.
//!
//! # Why these three methods
//!
//! 3GPP TS 24.229 §9.2.1 names two mechanisms: DHCPv6 (RFC 3319) and the
//! Protocol Configuration Options delivered at PDN activation. DNS on the home
//! realm is the TS 23.228 fallback. The original plan listed the IPv6 Router
//! Advertisement instead of the PCO, which was a mistake: an RA carries no
//! standard P-CSCF option, and the RA is already consumed by `netcfg` for the
//! default route. The PCO is the mechanism that genuinely should carry this.
//!
//! Ordering is cheapest-and-most-likely first: DHCPv6 gets a real answer from
//! a real server, the PCO is a single AT command, and DNS is last because it
//! cannot even be attempted without a resolver — which this carrier does not
//! provide.

use crate::error::BridgeResult;
use crate::modules::at_commander::{AtCommander, AtResponse};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::{Duration, Instant};

/// DHCPv6 message type for INFORMATION-REQUEST (RFC 8415).
const DHCP6_INFORMATION_REQUEST: u8 = 11;
/// DHCPv6 message type for REPLY.
const DHCP6_REPLY: u8 = 7;
/// RFC 3319 — SIP Servers Domain Name List.
const OPT_SIP_SERVER_DOMAINS: u16 = 21;
/// RFC 3319 — SIP Servers IPv6 Address List. The option we actually want.
const OPT_SIP_SERVER_ADDRS: u16 = 22;
/// RFC 3646 — DNS Recursive Name Servers. Requested so the DNS probe has
/// somewhere to aim, and so an all-zero answer is visible in the report.
const OPT_DNS_SERVERS: u16 = 23;
const OPT_CLIENT_ID: u16 = 1;
const OPT_ORO: u16 = 6;
const OPT_ELAPSED_TIME: u16 = 8;

/// All-DHCP-relay-agents-and-servers.
const DHCP6_SERVERS: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 2);
const DHCP6_CLIENT_PORT: u16 = 546;
const DHCP6_SERVER_PORT: u16 = 547;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryMethod {
    /// Operator-supplied. Not a probe — it short-circuits the chain (FR-010).
    ConfigOverride,
    /// DHCPv6 Information-Request, RFC 3319 options 21/22.
    Dhcpv6,
    /// Protocol Configuration Options, read over AT.
    Pco,
    /// NAPTR / AAAA on the home network realm.
    Dns,
}

impl DiscoveryMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            DiscoveryMethod::ConfigOverride => "config-override",
            DiscoveryMethod::Dhcpv6 => "dhcpv6",
            DiscoveryMethod::Pco => "pco",
            DiscoveryMethod::Dns => "dns",
        }
    }

    /// The chain, in the order it is attempted (FR-008).
    pub fn chain() -> [DiscoveryMethod; 3] {
        [
            DiscoveryMethod::Dhcpv6,
            DiscoveryMethod::Pco,
            DiscoveryMethod::Dns,
        ]
    }
}

/// The three-way outcome of one probe.
///
/// `NoResult` and `Failed` are deliberately distinct: "the carrier answered
/// and had nothing for us" is a completely different diagnosis from "we could
/// not ask", and collapsing them is what makes discovery failures unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodResult {
    Found(IpAddr),
    /// The probe ran; the carrier provided nothing. Carries what *was* seen.
    NoResult(String),
    /// The probe could not run.
    Failed(String),
}

impl MethodResult {
    pub fn found(&self) -> Option<IpAddr> {
        match self {
            MethodResult::Found(a) => Some(*a),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            MethodResult::Found(a) => format!("found {a}"),
            MethodResult::NoResult(d) => format!("no result — {d}"),
            MethodResult::Failed(d) => format!("could not run — {d}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MethodAttempt {
    pub method: DiscoveryMethod,
    pub result: MethodResult,
    pub duration: Duration,
}

/// Everything a discovery run learned. Produced whether or not an address was
/// found — that completeness is the requirement (FR-011).
#[derive(Debug, Clone, Default)]
pub struct DiscoveryReport {
    pub attempts: Vec<MethodAttempt>,
    pub outcome: Option<(IpAddr, DiscoveryMethod)>,
}

impl DiscoveryReport {
    pub fn record(&mut self, method: DiscoveryMethod, result: MethodResult, duration: Duration) {
        if let (None, Some(addr)) = (self.outcome, result.found()) {
            self.outcome = Some((addr, method));
        }
        self.attempts.push(MethodAttempt {
            method,
            result,
            duration,
        });
    }

    /// Operator-facing per-method breakdown.
    pub fn summary(&self) -> String {
        let mut s = String::from("P-CSCF discovery:\n");
        for a in &self.attempts {
            s.push_str(&format!(
                "  {:<16} {} ({} ms)\n",
                a.method.as_str(),
                a.result.describe(),
                a.duration.as_millis()
            ));
        }
        match self.outcome {
            Some((addr, method)) => {
                s.push_str(&format!("\nUsing {addr} (source: {}).\n", method.as_str()))
            }
            None => s.push_str(
                "\nNo P-CSCF discovered. This is the expected result on a carrier that \
                 does not publish one — supply an address explicitly with --pcscf.\n\
                 See specs/015-volte-host-ims/plan.md Gate G3.\n",
            ),
        }
        s
    }
}

// ---------------------------------------------------------------------------
// DHCPv6
// ---------------------------------------------------------------------------

fn push_option(buf: &mut Vec<u8>, code: u16, data: &[u8]) {
    buf.extend_from_slice(&code.to_be_bytes());
    buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
    buf.extend_from_slice(data);
}

/// Builds a DHCPv6 INFORMATION-REQUEST asking for the SIP-server options.
///
/// `duid_ll` is a DUID-LL (RFC 8415 §11.4): type 3, hardware type 1, MAC.
pub fn build_information_request(xid: [u8; 3], mac: &[u8; 6]) -> Vec<u8> {
    let mut msg = vec![DHCP6_INFORMATION_REQUEST, xid[0], xid[1], xid[2]];

    let mut duid = Vec::with_capacity(10);
    duid.extend_from_slice(&3u16.to_be_bytes()); // DUID-LL
    duid.extend_from_slice(&1u16.to_be_bytes()); // Ethernet
    duid.extend_from_slice(mac);
    push_option(&mut msg, OPT_CLIENT_ID, &duid);

    push_option(&mut msg, OPT_ELAPSED_TIME, &0u16.to_be_bytes());

    let mut oro = Vec::new();
    for code in [
        OPT_SIP_SERVER_DOMAINS,
        OPT_SIP_SERVER_ADDRS,
        OPT_DNS_SERVERS,
    ] {
        oro.extend_from_slice(&code.to_be_bytes());
    }
    push_option(&mut msg, OPT_ORO, &oro);

    msg
}

/// Splits a DHCPv6 message body into (code, value) options.
///
/// Returns `None` when the message is not a REPLY or is malformed, rather than
/// silently yielding an empty option list — the difference matters to the
/// report.
pub fn parse_dhcp6_reply(msg: &[u8]) -> Option<Vec<(u16, Vec<u8>)>> {
    if msg.len() < 4 || msg[0] != DHCP6_REPLY {
        return None;
    }
    let mut options = Vec::new();
    let mut i = 4;
    while i + 4 <= msg.len() {
        let code = u16::from_be_bytes([msg[i], msg[i + 1]]);
        let len = u16::from_be_bytes([msg[i + 2], msg[i + 3]]) as usize;
        i += 4;
        if i + len > msg.len() {
            break;
        }
        options.push((code, msg[i..i + len].to_vec()));
        i += len;
    }
    Some(options)
}

/// Extracts usable SIP-server addresses from option 22.
///
/// All-zero addresses are dropped: this carrier returns `::` for DNS servers,
/// and treating such a value as an answer would be worse than reporting none.
pub fn extract_sip_servers(options: &[(u16, Vec<u8>)]) -> Vec<Ipv6Addr> {
    options
        .iter()
        .filter(|(code, _)| *code == OPT_SIP_SERVER_ADDRS)
        .flat_map(|(_, val)| {
            val.chunks_exact(16).filter_map(|c| {
                let mut o = [0u8; 16];
                o.copy_from_slice(c);
                let a = Ipv6Addr::from(o);
                (!a.is_unspecified()).then_some(a)
            })
        })
        .collect()
}

/// Addresses carried in the DNS-servers option, for the DNS probe to aim at.
pub fn extract_dns_servers(options: &[(u16, Vec<u8>)]) -> Vec<Ipv6Addr> {
    options
        .iter()
        .filter(|(code, _)| *code == OPT_DNS_SERVERS)
        .flat_map(|(_, val)| {
            val.chunks_exact(16).filter_map(|c| {
                let mut o = [0u8; 16];
                o.copy_from_slice(c);
                let a = Ipv6Addr::from(o);
                (!a.is_unspecified()).then_some(a)
            })
        })
        .collect()
}

/// Renders what a reply contained, so an empty result is still informative.
pub fn describe_options(options: &[(u16, Vec<u8>)]) -> String {
    if options.is_empty() {
        return "reply carried no options".to_string();
    }
    let names: Vec<String> = options
        .iter()
        .map(|(code, val)| match *code {
            OPT_CLIENT_ID => "client-id".to_string(),
            2 => "server-id".to_string(),
            OPT_SIP_SERVER_DOMAINS => format!("sip-domains[{}B]", val.len()),
            OPT_SIP_SERVER_ADDRS => format!("sip-addresses[{}B]", val.len()),
            OPT_DNS_SERVERS => {
                let addrs = extract_dns_servers(&[(*code, val.clone())]);
                if addrs.is_empty() {
                    "dns-servers(all zero)".to_string()
                } else {
                    format!("dns-servers{addrs:?}")
                }
            }
            other => format!("option-{other}"),
        })
        .collect();
    format!("reply carried: {}", names.join(", "))
}

fn read_ifindex(iface: &str) -> std::io::Result<u32> {
    let raw = std::fs::read_to_string(format!("/sys/class/net/{iface}/ifindex"))?;
    raw.trim()
        .parse()
        .map_err(|e| std::io::Error::other(format!("bad ifindex: {e}")))
}

fn read_mac(iface: &str) -> std::io::Result<[u8; 6]> {
    let raw = std::fs::read_to_string(format!("/sys/class/net/{iface}/address"))?;
    let bytes: Vec<u8> = raw
        .trim()
        .split(':')
        .filter_map(|b| u8::from_str_radix(b, 16).ok())
        .collect();
    if bytes.len() != 6 {
        return Err(std::io::Error::other("bad MAC"));
    }
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&bytes);
    Ok(mac)
}

/// Sends a DHCPv6 Information-Request on `iface` and reports what came back.
pub fn probe_dhcpv6(iface: &str, timeout: Duration) -> MethodResult {
    let ifindex = match read_ifindex(iface) {
        Ok(i) => i,
        Err(e) => return MethodResult::Failed(format!("cannot read ifindex for {iface}: {e}")),
    };
    let mac = match read_mac(iface) {
        Ok(m) => m,
        Err(e) => return MethodResult::Failed(format!("cannot read MAC for {iface}: {e}")),
    };

    // Sending to a link-local multicast group needs a usable link-local source
    // address. Without this check the send fails as a bare "Network is
    // unreachable", which reads like a routing problem and is not one.
    match super::netcfg::link_local_ready(iface) {
        Ok(true) => {}
        Ok(false) => {
            return MethodResult::Failed(format!(
                "{iface} has no link-local address ready (still tentative, or absent); \
                 bring the IMS PDN up first with `volte-pdn --action up`"
            ))
        }
        Err(e) => return MethodResult::Failed(format!("cannot inspect {iface}: {e}")),
    }

    let socket = match socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    ) {
        Ok(s) => s,
        Err(e) => return MethodResult::Failed(format!("socket() failed: {e}")),
    };
    let _ = socket.set_reuse_address(true);
    if let Err(e) = socket.set_multicast_if_v6(ifindex) {
        return MethodResult::Failed(format!("IPV6_MULTICAST_IF failed: {e}"));
    }
    let bind: SocketAddr =
        SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, DHCP6_CLIENT_PORT, 0, ifindex).into();
    if let Err(e) = socket.bind(&bind.into()) {
        return MethodResult::Failed(format!("bind to :{DHCP6_CLIENT_PORT} failed: {e}"));
    }
    let _ = socket.set_read_timeout(Some(timeout));

    // socket2 is needed only for IPV6_MULTICAST_IF; hand the fd to std for the
    // actual I/O so the receive path takes an initialised `&mut [u8]` and no
    // `unsafe` is required (this crate is zero-unsafe by policy, enforced by
    // `make lint`). socket2's own `recv` takes `MaybeUninit`, which would.
    let socket: std::net::UdpSocket = socket.into();

    let xid = [
        std::process::id() as u8,
        (std::process::id() >> 8) as u8,
        0x42,
    ];
    let msg = build_information_request(xid, &mac);
    let dest: SocketAddr = SocketAddrV6::new(DHCP6_SERVERS, DHCP6_SERVER_PORT, 0, ifindex).into();
    if let Err(e) = socket.send_to(&msg, dest) {
        return MethodResult::Failed(format!("send to {dest} failed: {e}"));
    }

    let mut buf = [0u8; 2048];
    let n = match socket.recv(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            return MethodResult::NoResult(format!("no DHCPv6 reply within {timeout:?} ({e})"))
        }
    };
    let data = &buf[..n];

    match parse_dhcp6_reply(data) {
        None => MethodResult::NoResult("received a non-REPLY DHCPv6 message".to_string()),
        Some(options) => {
            let servers = extract_sip_servers(&options);
            match servers.first() {
                Some(addr) => MethodResult::Found(IpAddr::V6(*addr)),
                None => MethodResult::NoResult(format!(
                    "server answered but provided no RFC 3319 SIP-server options; {}",
                    describe_options(&options)
                )),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PCO (over AT)
// ---------------------------------------------------------------------------

/// Extracts P-CSCF addresses from a `+CGCONTRDP` line.
///
/// TS 27.007 places the P-CSCF fields after the two DNS fields (indices 7 and
/// 8, zero-based). On the reference firmware the line simply ends before them,
/// which is why the PCO probe reports "firmware truncates the response" rather
/// than "the carrier sent nothing" — a distinction that points at the right
/// culprit.
pub fn parse_cgcontrdp_pcscf(lines: &[String], cid: u8) -> Result<Vec<IpAddr>, String> {
    let mut saw_line = false;
    let mut best: Vec<IpAddr> = Vec::new();
    let mut max_fields = 0usize;

    for line in lines {
        let Some(payload) = line.trim().strip_prefix("+CGCONTRDP:") else {
            continue;
        };
        let f = super::pdn::split_at_fields(payload);
        if f.is_empty() || f[0].parse::<u8>() != Ok(cid) {
            continue;
        }
        saw_line = true;
        max_fields = max_fields.max(f.len());
        for field in f.iter().skip(7).take(2) {
            if let Some(addr) = super::pdn::dotted_to_ipv6(field) {
                if !addr.is_unspecified() {
                    best.push(IpAddr::V6(addr));
                }
            } else if let Ok(addr) = field.parse::<IpAddr>() {
                if !addr.is_unspecified() {
                    best.push(addr);
                }
            }
        }
    }

    if !saw_line {
        return Err(format!("no +CGCONTRDP line for context {cid}"));
    }
    if best.is_empty() && max_fields <= 7 {
        return Err(format!(
            "firmware truncates +CGCONTRDP after the DNS fields ({max_fields} fields; \
             the P-CSCF fields would be 8 and 9), so the PCO is not exposed"
        ));
    }
    Ok(best)
}

/// Reads the PCO through AT and reports whether it carries a P-CSCF.
pub fn probe_pco(at: &mut AtCommander, cid: u8) -> MethodResult {
    let lines = match at.send_command(&format!("AT+CGCONTRDP={cid}")) {
        Ok(AtResponse::Ok(l)) => l,
        Ok(AtResponse::Error(e)) => {
            return MethodResult::Failed(format!("AT+CGCONTRDP returned {e}"))
        }
        Ok(AtResponse::CmeError(c, m)) => {
            return MethodResult::Failed(format!("AT+CGCONTRDP returned +CME ERROR: {c} ({m})"))
        }
        Err(e) => return MethodResult::Failed(format!("AT+CGCONTRDP failed: {e}")),
    };

    match parse_cgcontrdp_pcscf(&lines, cid) {
        Ok(addrs) => match addrs.first() {
            Some(addr) => MethodResult::Found(*addr),
            None => MethodResult::NoResult(
                "+CGCONTRDP exposed the P-CSCF fields but they were empty".to_string(),
            ),
        },
        Err(detail) => MethodResult::NoResult(detail),
    }
}

// ---------------------------------------------------------------------------
// DNS
// ---------------------------------------------------------------------------

/// Builds a DNS query for `qname`/`qtype`.
pub fn build_dns_query(xid: u16, qname: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::new();
    q.extend_from_slice(&xid.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // standard query, recursion desired
    q.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // an/ns/ar count
    for label in qname.split('.').filter(|l| !l.is_empty()) {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // IN
    q
}

/// Number of answer records in a DNS response.
pub fn dns_answer_count(msg: &[u8]) -> Option<u16> {
    (msg.len() >= 8).then(|| u16::from_be_bytes([msg[6], msg[7]]))
}

/// The standard 3GPP home-network realm for a PLMN.
pub fn home_realm(mcc: &str, mnc: &str) -> String {
    format!("ims.mnc{mnc}.mcc{mcc}.3gppnetwork.org")
}

/// Attempts DNS discovery against whatever resolvers the PDN offered.
pub fn probe_dns(resolvers: &[Ipv6Addr], realm: &str, timeout: Duration) -> MethodResult {
    if resolvers.is_empty() {
        // Not a failure of DNS as such — there is simply nowhere to ask. Saying
        // so is more useful than a timeout would be.
        return MethodResult::NoResult(
            "no resolver is offered on the IMS PDN, so NAPTR/AAAA cannot be queried".to_string(),
        );
    }
    let socket = match std::net::UdpSocket::bind("[::]:0") {
        Ok(s) => s,
        Err(e) => return MethodResult::Failed(format!("UDP bind failed: {e}")),
    };
    let _ = socket.set_read_timeout(Some(timeout));

    for resolver in resolvers {
        let dest = SocketAddr::from(SocketAddrV6::new(*resolver, 53, 0, 0));
        let query = build_dns_query(0x1234, realm, 35 /* NAPTR */);
        if socket.send_to(&query, dest).is_err() {
            continue;
        }
        let mut buf = [0u8; 1500];
        match socket.recv(&mut buf) {
            Ok(n) => match dns_answer_count(&buf[..n]) {
                Some(0) | None => {
                    return MethodResult::NoResult(format!(
                        "resolver {resolver} answered with no NAPTR records for {realm}"
                    ))
                }
                Some(count) => {
                    return MethodResult::NoResult(format!(
                        "resolver {resolver} returned {count} NAPTR record(s) for {realm}; \
                         resolving them to a P-CSCF address is not implemented — \
                         supply the address with --pcscf"
                    ))
                }
            },
            Err(e) => {
                return MethodResult::NoResult(format!("resolver {resolver} did not answer: {e}"))
            }
        }
    }
    MethodResult::NoResult("no resolver answered".to_string())
}

// ---------------------------------------------------------------------------
// The chain
// ---------------------------------------------------------------------------

/// Inputs the chain needs that it cannot discover for itself.
pub struct DiscoveryInputs<'a> {
    pub iface: &'a str,
    pub cid: u8,
    pub modem_port: &'a std::path::Path,
    /// Home realm for the DNS probe, if the PLMN is known.
    pub realm: Option<String>,
    /// FR-010 override. When present, the probes are skipped entirely.
    pub override_pcscf: Option<IpAddr>,
    /// Restrict the run to a single method, for isolating one probe.
    pub only: Option<DiscoveryMethod>,
}

/// Runs the discovery chain and returns a complete report (FR-008, FR-011).
pub fn discover(inputs: &DiscoveryInputs) -> BridgeResult<DiscoveryReport> {
    let mut report = DiscoveryReport::default();

    if let Some(addr) = inputs.override_pcscf {
        report.record(
            DiscoveryMethod::ConfigOverride,
            MethodResult::Found(addr),
            Duration::ZERO,
        );
        return Ok(report);
    }

    for method in DiscoveryMethod::chain() {
        if inputs.only.is_some_and(|m| m != method) {
            continue;
        }
        let started = Instant::now();
        let result = match method {
            DiscoveryMethod::Dhcpv6 => {
                if inputs.iface.is_empty() {
                    MethodResult::Failed("no interface given".to_string())
                } else {
                    probe_dhcpv6(inputs.iface, Duration::from_secs(6))
                }
            }
            DiscoveryMethod::Pco => match AtCommander::open(inputs.modem_port) {
                Ok(mut at) => probe_pco(&mut at, inputs.cid),
                Err(e) => MethodResult::Failed(format!("cannot open the modem: {e}")),
            },
            DiscoveryMethod::Dns => {
                let realm = inputs.realm.clone().unwrap_or_default();
                if realm.is_empty() {
                    MethodResult::Failed("home realm unknown".to_string())
                } else {
                    // Resolvers would come from the DHCPv6 reply; this carrier
                    // offers none, which the probe reports rather than hangs on.
                    probe_dns(&[], &realm, Duration::from_secs(4))
                }
            }
            DiscoveryMethod::ConfigOverride => continue,
        };
        report.record(method, result, started.elapsed());
        // Keep probing even after a hit: the full breakdown is the deliverable
        // (FR-011), and later methods are cheap.
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn information_request_asks_for_the_sip_server_options() {
        let msg = build_information_request([1, 2, 3], &[0x02, 0x4b, 0xb3, 0xb9, 0xeb, 0xe5]);

        assert_eq!(msg[0], DHCP6_INFORMATION_REQUEST);
        assert_eq!(&msg[1..4], &[1, 2, 3]);
        let options = parse_options_of_request(&msg);
        let oro = options
            .iter()
            .find(|(c, _)| *c == OPT_ORO)
            .expect("no option-request");
        let requested: Vec<u16> = oro
            .1
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        assert!(requested.contains(&OPT_SIP_SERVER_ADDRS));
        assert!(requested.contains(&OPT_SIP_SERVER_DOMAINS));
    }

    /// Request-side option walker (the reply parser insists on msg-type REPLY).
    fn parse_options_of_request(msg: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 4;
        while i + 4 <= msg.len() {
            let code = u16::from_be_bytes([msg[i], msg[i + 1]]);
            let len = u16::from_be_bytes([msg[i + 2], msg[i + 3]]) as usize;
            i += 4;
            out.push((code, msg[i..i + len].to_vec()));
            i += len;
        }
        out
    }

    #[test]
    fn embeds_a_duid_ll_built_from_the_interface_mac() {
        let msg = build_information_request([0, 0, 0], &[0x02, 0x4b, 0xb3, 0xb9, 0xeb, 0xe5]);

        let opts = parse_options_of_request(&msg);
        let duid = &opts.iter().find(|(c, _)| *c == OPT_CLIENT_ID).unwrap().1;
        assert_eq!(&duid[0..2], &3u16.to_be_bytes()); // DUID-LL
        assert_eq!(&duid[2..4], &1u16.to_be_bytes()); // Ethernet
        assert_eq!(&duid[4..10], &[0x02, 0x4b, 0xb3, 0xb9, 0xeb, 0xe5]);
    }

    #[test]
    fn rejects_a_message_that_is_not_a_reply() {
        let not_a_reply = vec![DHCP6_INFORMATION_REQUEST, 0, 0, 0];

        assert!(parse_dhcp6_reply(&not_a_reply).is_none());
    }

    #[test]
    fn parses_the_reply_this_carrier_actually_sent() {
        // Reconstructed from the G1 capture: a REPLY carrying client-id,
        // server-id and DNS servers of `::` — and no SIP options at all.
        let mut msg = vec![DHCP6_REPLY, 0xe8, 0x2a, 0x05];
        push_option(&mut msg, OPT_CLIENT_ID, &[0u8; 10]);
        push_option(&mut msg, 2, &[0u8; 10]);
        push_option(&mut msg, OPT_DNS_SERVERS, &[0u8; 32]); // two `::` entries

        let options = parse_dhcp6_reply(&msg).expect("should parse");

        assert!(
            extract_sip_servers(&options).is_empty(),
            "carrier sent no SIP options"
        );
        assert!(
            extract_dns_servers(&options).is_empty(),
            "all-zero DNS servers must not count as answers"
        );
        assert!(describe_options(&options).contains("dns-servers(all zero)"));
    }

    #[test]
    fn extracts_sip_servers_when_a_carrier_does_provide_them() {
        let addr: Ipv6Addr = "2402:8100::5".parse().unwrap();
        let mut msg = vec![DHCP6_REPLY, 0, 0, 0];
        push_option(&mut msg, OPT_SIP_SERVER_ADDRS, &addr.octets());

        let options = parse_dhcp6_reply(&msg).unwrap();

        assert_eq!(extract_sip_servers(&options), vec![addr]);
    }

    #[test]
    fn truncated_options_do_not_panic() {
        let msg = vec![DHCP6_REPLY, 0, 0, 0, 0, 22, 0, 200, 1, 2, 3];

        let options = parse_dhcp6_reply(&msg).unwrap();

        assert!(options.is_empty());
    }

    #[test]
    fn pco_probe_blames_the_firmware_when_the_response_is_truncated() {
        // The reference firmware stops after the DNS fields, so the P-CSCF
        // fields never appear. That is a firmware limitation, not the carrier
        // withholding the address, and the message must say which.
        let lines = vec![
            "+CGCONTRDP: 3,6,\"ims.mnc043.mcc404.gprs\",\"36.2.129.0\",\"254.128.0.0\",\"0.0.0.0\",\"0.0.0.0\"".to_string(),
        ];

        let err = parse_cgcontrdp_pcscf(&lines, 3).unwrap_err();

        assert!(err.contains("truncates"), "got: {err}");
        assert!(err.contains("PCO is not exposed"), "got: {err}");
    }

    #[test]
    fn pco_probe_reads_the_pcscf_when_firmware_does_expose_it() {
        // Nine fields: the last two are the P-CSCF pair, in dotted-byte form.
        let pcscf = "36.2.129.0.0.0.0.0.0.0.0.0.0.0.0.5";
        let lines = vec![format!(
            "+CGCONTRDP: 3,6,\"ims.x\",\"36.2.129.0\",\"254.128.0.0\",\"0.0.0.0\",\"0.0.0.0\",\"{pcscf}\",\"{pcscf}\""
        )];

        let addrs = parse_cgcontrdp_pcscf(&lines, 3).unwrap();

        assert_eq!(addrs.len(), 2);
        assert_eq!(addrs[0], "2402:8100::5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn pco_probe_reports_a_missing_context_distinctly() {
        let err = parse_cgcontrdp_pcscf(&[], 3).unwrap_err();

        assert!(err.contains("no +CGCONTRDP line"));
    }

    #[test]
    fn dns_probe_says_there_is_nowhere_to_ask_rather_than_timing_out() {
        let r = probe_dns(&[], "ims.mnc043.mcc404.3gppnetwork.org", Duration::ZERO);

        match r {
            MethodResult::NoResult(d) => assert!(d.contains("no resolver")),
            other => panic!("expected NoResult, got {other:?}"),
        }
    }

    #[test]
    fn builds_a_well_formed_dns_query() {
        let q = build_dns_query(0x1234, "ims.mnc043.mcc404.3gppnetwork.org", 35);

        assert_eq!(&q[0..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([q[4], q[5]]), 1, "qdcount");
        assert_eq!(q[12], 3, "first label 'ims' length");
        assert_eq!(&q[13..16], b"ims");
        assert_eq!(&q[q.len() - 4..], &[0, 35, 0, 1], "qtype NAPTR, class IN");
    }

    #[test]
    fn derives_the_standard_home_realm() {
        assert_eq!(
            home_realm("404", "043"),
            "ims.mnc043.mcc404.3gppnetwork.org"
        );
    }

    #[test]
    fn config_override_short_circuits_the_chain() {
        let addr: IpAddr = "2402:8100::1".parse().unwrap();
        let inputs = DiscoveryInputs {
            iface: "",
            cid: 3,
            modem_port: std::path::Path::new("/nonexistent"),
            realm: None,
            override_pcscf: Some(addr),
            only: None,
        };

        let report = discover(&inputs).unwrap();

        assert_eq!(
            report.outcome,
            Some((addr, DiscoveryMethod::ConfigOverride))
        );
        assert_eq!(report.attempts.len(), 1, "probes must not run");
    }

    #[test]
    fn the_chain_is_attempted_in_the_documented_order() {
        assert_eq!(
            DiscoveryMethod::chain(),
            [
                DiscoveryMethod::Dhcpv6,
                DiscoveryMethod::Pco,
                DiscoveryMethod::Dns
            ]
        );
    }

    #[test]
    fn every_method_is_reported_even_when_all_fail() {
        let mut report = DiscoveryReport::default();
        report.record(
            DiscoveryMethod::Dhcpv6,
            MethodResult::NoResult("no SIP options".into()),
            Duration::from_millis(12),
        );
        report.record(
            DiscoveryMethod::Pco,
            MethodResult::NoResult("truncated".into()),
            Duration::from_millis(3),
        );
        report.record(
            DiscoveryMethod::Dns,
            MethodResult::Failed("no resolver".into()),
            Duration::ZERO,
        );

        let s = report.summary();

        assert!(s.contains("dhcpv6"));
        assert!(s.contains("pco"));
        assert!(s.contains("dns"));
        assert!(s.contains("No P-CSCF discovered"));
        assert!(
            s.contains("--pcscf"),
            "must point at the working alternative"
        );
        assert!(report.outcome.is_none());
    }

    #[test]
    fn the_first_hit_wins_but_later_probes_still_report() {
        let mut report = DiscoveryReport::default();
        let a: IpAddr = "2402:8100::5".parse().unwrap();
        let b: IpAddr = "2402:8100::9".parse().unwrap();

        report.record(
            DiscoveryMethod::Dhcpv6,
            MethodResult::Found(a),
            Duration::ZERO,
        );
        report.record(DiscoveryMethod::Pco, MethodResult::Found(b), Duration::ZERO);

        assert_eq!(report.outcome, Some((a, DiscoveryMethod::Dhcpv6)));
        assert_eq!(report.attempts.len(), 2);
    }

    #[test]
    fn result_descriptions_separate_empty_from_unrunnable() {
        assert!(MethodResult::NoResult("x".into())
            .describe()
            .contains("no result"));
        assert!(MethodResult::Failed("x".into())
            .describe()
            .contains("could not run"));
    }
}
