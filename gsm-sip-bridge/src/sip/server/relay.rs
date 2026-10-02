//! Proxy mode for a registered phone's dial-out INVITE
//! (`[outbound].sip_server_dial_mode = "proxy"`).
//!
//! The registrar relays the INVITE to the pjsua-hosted dial-out account and
//! passes that account's responses back to the phone, instead of answering
//! `302` and trusting the handset to follow it — PJSIP-based softphones
//! (Telephone.app, among others) treat a 3xx as a failed call and never
//! re-INVITE.
//!
//! **No Record-Route, on purpose.** Only the INVITE transaction crosses this
//! relay (INVITE, CANCEL, and the ACK for a non-2xx final response). Once the
//! call is answered the phone and the dial-out account talk directly — the
//! account's `200 OK` carries its own `Contact` — so BYE, re-INVITE, the ACK
//! for the 2xx and all media bypass the registrar. There is no dialog state
//! to keep, expire or leak.
//!
//! The relay sends from its own socket so the dial-out account sees a
//! distinct source it can recognise ([`BindingStore::is_trusted_dialout_source`]):
//! the registrar has already verified the phone against its binding before
//! anything is relayed.
//!
//! [`BindingStore::is_trusted_dialout_source`]: super::BindingStore::is_trusted_dialout_source

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{BindingStore, MAX_DATAGRAM, READ_TIMEOUT};

/// How long a relayed INVITE is remembered while nothing has been heard back,
/// and after each provisional response. RFC 3261 §16.6 step 11 Timer C: a
/// proxy waits at least three minutes for a final response, and every
/// provisional response restarts the clock — a call that rings for a minute
/// must still be able to deliver its `200 OK`.
const PROVISIONAL_TTL: Duration = Duration::from_secs(180);

/// After a 2xx: long enough to pass its retransmissions until the phone's
/// direct ACK silences them (RFC 3261 Timer B, 64 × T1).
const SUCCESS_TTL: Duration = Duration::from_secs(64);

/// After a 3xx–6xx: long enough for the phone's ACK and any retransmission.
const FAILURE_TTL: Duration = Duration::from_secs(32);

/// Why a request could not be relayed.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum RelayError {
    /// `Max-Forwards` was already 0 (RFC 3261 §16.3).
    TooManyHops,
    /// The request had no usable top `Via` branch or `Call-ID`.
    Malformed,
    /// The dial-out account could not be reached.
    Unreachable,
}

struct Entry {
    peer: SocketAddr,
    /// Where this transaction's requests went, and the only address its
    /// responses are accepted from.
    target: SocketAddr,
    expires: Instant,
}

pub(super) struct Relay {
    socket: UdpSocket,
    /// The dial-out account's UDP port (`[sip].local_port`).
    account_port: u16,
    /// The relay socket's own port.
    local_port: u16,
    /// A specific `listen_addr`: the dial-out account is always reached on it.
    /// `None` for a wildcard — the address is then whichever local one routes
    /// to the phone, which needs no DNS (the realm need not even resolve).
    fixed_ip: Option<IpAddr>,
    /// The wildcard of the relay socket's family, for route probing.
    wildcard: IpAddr,
    bindings: Arc<BindingStore>,
    transactions: Mutex<HashMap<String, Entry>>,
}

impl Relay {
    /// Binds the relay socket in the same family as the registrar's own
    /// `listen_addr`.
    pub(super) fn bind(
        listen_addr: &str,
        account_port: u16,
        bindings: Arc<BindingStore>,
    ) -> std::io::Result<Self> {
        let (fixed_ip, bind_ip) = match listen_addr.parse::<IpAddr>() {
            Ok(ip) if ip.is_unspecified() => (None, ip),
            Ok(ip) => (Some(ip), ip),
            // A hostname the registrar itself already bound by name.
            Err(_) => {
                let ip = (listen_addr, 0)
                    .to_socket_addrs()?
                    .next()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::AddrNotAvailable,
                            format!("no address for {listen_addr}"),
                        )
                    })?
                    .ip();
                (Some(ip), ip)
            }
        };
        // Bound to the specific address, never the wildcard, whenever there is
        // one: the dial-out account's reply and the Contact it advertises then
        // resolve to the LAN address, not 127.0.0.1.
        let socket = UdpSocket::bind((bind_ip, 0))?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        let wildcard = match bind_ip {
            IpAddr::V4(_) => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
        };
        Ok(Self {
            local_port: socket.local_addr()?.port(),
            socket,
            account_port,
            fixed_ip,
            wildcard,
            bindings,
            transactions: Mutex::new(HashMap::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.transactions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The local address the dial-out account is reached on for `peer`.
    fn route_ip(&self, peer: SocketAddr) -> std::io::Result<IpAddr> {
        if let Some(ip) = self.fixed_ip {
            return Ok(ip);
        }
        // A connected UDP socket sends nothing; it only asks the kernel which
        // local address routes to `peer`.
        let probe = UdpSocket::bind((self.wildcard, 0))?;
        probe.connect(peer)?;
        Ok(probe.local_addr()?.ip())
    }

    /// Relays an INVITE from `peer`, remembering where its responses go.
    pub(super) fn forward_invite(
        &self,
        text: &str,
        peer: SocketAddr,
        now: Instant,
    ) -> Result<(), RelayError> {
        let branch = relay_branch(text).ok_or(RelayError::Malformed)?;
        let ip = self.route_ip(peer).map_err(|e| {
            tracing::warn!(%peer, error = %e, "sip_server: no route from the relay to the phone's network");
            RelayError::Unreachable
        })?;
        let target = SocketAddr::new(ip, self.account_port);
        let rewritten = rewrite_request(text, ip, self.local_port, &branch)?;
        // Before sending, so the dial-out account can never see a relayed
        // request from a source it does not yet trust.
        self.bindings
            .add_relay_source(SocketAddr::new(ip, self.local_port));
        self.lock().insert(
            branch,
            Entry {
                peer,
                target,
                expires: now + PROVISIONAL_TTL,
            },
        );
        self.socket
            .send_to(rewritten.as_bytes(), target)
            .map(|_| ())
            .map_err(|e| {
                tracing::warn!(error = %e, %target, "sip_server: relay to the dial-out account failed");
                RelayError::Unreachable
            })
    }

    /// Relays a CANCEL, or the ACK for a non-2xx final response, belonging to
    /// an INVITE this relay forwarded. `Ok(false)` when no such transaction
    /// exists for `peer` — the caller decides how to answer that.
    pub(super) fn forward_in_transaction(
        &self,
        text: &str,
        peer: SocketAddr,
        now: Instant,
    ) -> Result<bool, RelayError> {
        let branch = relay_branch(text).ok_or(RelayError::Malformed)?;
        let target = self
            .lock()
            .get(&branch)
            .filter(|e| e.peer == peer && e.expires > now)
            .map(|e| e.target);
        let Some(target) = target else {
            return Ok(false);
        };
        let rewritten = rewrite_request(text, target.ip(), self.local_port, &branch)?;
        self.socket
            .send_to(rewritten.as_bytes(), target)
            .map(|_| true)
            .map_err(|_| RelayError::Unreachable)
    }

    /// Reads the relay socket until `stop`, passing each response from the
    /// dial-out account back to the phone through `out` (the registrar's own
    /// socket, so the phone sees them from the port it registered to).
    pub(super) fn run(&self, out: &UdpSocket, stop: &AtomicBool) {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        while !stop.load(Ordering::SeqCst) {
            match self.socket.recv_from(&mut buf) {
                Ok((len, from)) => {
                    let Ok(text) = std::str::from_utf8(&buf[..len]) else {
                        continue;
                    };
                    if let Some((peer, response)) =
                        self.response_for_phone(text, from, Instant::now())
                    {
                        let response =
                            crate::ims::sip_client::annotate_via_received_rport(&response, peer);
                        if let Err(e) = out.send_to(response.as_bytes(), peer) {
                            tracing::warn!(%peer, error = %e, "sip_server: failed to pass a relayed response to the phone");
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    let now = Instant::now();
                    self.lock().retain(|_, e| e.expires > now);
                }
                Err(e) => tracing::warn!(error = %e, "sip_server: relay recv failed"),
            }
        }
    }

    /// For a response received from `from`: the phone it belongs to and the
    /// response with our `Via` removed. `None` for anything that is not a
    /// response to a live relayed transaction *from the address that
    /// transaction was sent to*. Also slides the transaction's lifetime.
    fn response_for_phone(
        &self,
        text: &str,
        from: SocketAddr,
        now: Instant,
    ) -> Option<(SocketAddr, String)> {
        let status: u16 = text.strip_prefix("SIP/2.0 ")?.get(..3)?.parse().ok()?;
        let mut lines: Vec<&str> = text.split("\r\n").collect();
        let via_idx = lines.iter().position(|l| is_via(l))?;
        let branch = param(lines[via_idx], "branch")?;
        let peer = {
            let mut map = self.lock();
            let entry = map.get_mut(branch)?;
            if entry.expires <= now || entry.target != from {
                return None;
            }
            entry.expires = now
                + match status {
                    100..=199 => PROVISIONAL_TTL,
                    200..=299 => SUCCESS_TTL,
                    _ => FAILURE_TTL,
                };
            entry.peer
        };
        lines.remove(via_idx);
        Some((peer, lines.join("\r\n")))
    }
}

/// The relayed branch: deterministic in the phone's branch and `Call-ID`, so a
/// retransmitted INVITE, the CANCEL and the ACK for a non-2xx (which RFC 3261
/// gives the same branch) all land on the same transaction without any lookup
/// keyed on the phone's own text.
fn relay_branch(text: &str) -> Option<String> {
    let mut lines = text.split("\r\n");
    lines.next()?;
    let mut via_branch = None;
    let mut call_id = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        if via_branch.is_none() && is_via(line) {
            via_branch = param(line, "branch");
        } else if call_id.is_none() && (header_is(line, "call-id") || header_is(line, "i")) {
            call_id = line.split_once(':').map(|(_, v)| v.trim());
        }
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    via_branch?.hash(&mut hasher);
    call_id?.hash(&mut hasher);
    Some(format!("z9hG4bK-rl{:016x}", hasher.finish()))
}

/// Our `Via` in, `Max-Forwards` down by one. Everything else — Request-URI,
/// To, SDP — is passed through untouched: the dial-out account derives the
/// destination from `To`, and the phone's own SDP is what the call negotiates.
fn rewrite_request(
    text: &str,
    via_ip: IpAddr,
    via_port: u16,
    branch: &str,
) -> Result<String, RelayError> {
    let via_host = match via_ip.to_canonical() {
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    let mut out: Vec<String> = Vec::new();
    let mut in_headers = true;
    let mut saw_max_forwards = false;
    for (i, line) in text.split("\r\n").enumerate() {
        if i == 0 {
            out.push(line.to_string());
            out.push(format!(
                "Via: SIP/2.0/UDP {via_host}:{via_port};branch={branch}"
            ));
            continue;
        }
        if in_headers && line.is_empty() {
            in_headers = false;
            if !saw_max_forwards {
                // RFC 3261 §16.6 step 3: a request without one gets the default.
                out.push("Max-Forwards: 69".to_string());
            }
        }
        if in_headers && header_is(line, "max-forwards") {
            saw_max_forwards = true;
            let hops: u32 = line
                .split_once(':')
                .and_then(|(_, v)| v.trim().parse().ok())
                .ok_or(RelayError::Malformed)?;
            if hops == 0 {
                return Err(RelayError::TooManyHops);
            }
            out.push(format!("Max-Forwards: {}", hops - 1));
            continue;
        }
        out.push(line.to_string());
    }
    Ok(out.join("\r\n"))
}

/// `Via`, or its compact form `v` (RFC 3261 §7.3.3) — which the registrar's
/// own parser accepts, so a phone may send either.
fn is_via(line: &str) -> bool {
    header_is(line, "via") || header_is(line, "v")
}

/// True when `line` is the header `name` (case-insensitive, `name:` form).
fn header_is(line: &str, name: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(name))
}

/// The value of `;key=value` in a header line, without any trailing parameter.
fn param<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (_, rest) = line.split_once(';')?;
    rest.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case(key).then(|| v.trim())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "INVITE sip:+919000000000@bridge SIP/2.0\r\n\
        Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bKabc;rport\r\n\
        Max-Forwards: 70\r\n\
        Call-ID: c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    #[test]
    fn rewriting_adds_our_via_first_and_decrements_max_forwards() {
        let out = rewrite_request(INVITE, "10.0.0.1".parse().unwrap(), 6000, "z9hG4bK-x").unwrap();
        let lines: Vec<&str> = out.split("\r\n").collect();
        assert_eq!(lines[1], "Via: SIP/2.0/UDP 10.0.0.1:6000;branch=z9hG4bK-x");
        assert!(lines[2].contains("192.168.1.50"), "phone's Via stays below");
        assert!(out.contains("Max-Forwards: 69\r\n"));
        assert!(out.starts_with("INVITE sip:+919000000000@bridge SIP/2.0"));
    }

    #[test]
    fn zero_max_forwards_is_refused() {
        let text = INVITE.replace("Max-Forwards: 70", "Max-Forwards: 0");
        assert_eq!(
            rewrite_request(&text, "10.0.0.1".parse().unwrap(), 1, "b"),
            Err(RelayError::TooManyHops)
        );
    }

    #[test]
    fn a_missing_max_forwards_gets_the_default() {
        let text = INVITE.replace("Max-Forwards: 70\r\n", "");
        let out = rewrite_request(&text, "10.0.0.1".parse().unwrap(), 1, "b").unwrap();
        assert!(out.contains("Max-Forwards: 69\r\n"));
    }

    fn response(status: &str, relay_branch: &str) -> String {
        format!(
            "SIP/2.0 {status}\r\nVia: SIP/2.0/UDP 10.0.0.1:1;branch={relay_branch}\r\n\
             Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bKabc\r\nContent-Length: 0\r\n\r\n"
        )
    }

    /// A call may ring far longer than one transaction timer: each provisional
    /// response slides the lifetime, so a late `200 OK` still reaches the phone.
    #[test]
    fn a_late_answer_is_delivered_after_a_long_ring() {
        let account = UdpSocket::bind("127.0.0.1:0").unwrap();
        let account_addr = account.local_addr().unwrap();
        let relay = Relay::bind(
            "127.0.0.1",
            account_addr.port(),
            Arc::new(BindingStore::new()),
        )
        .unwrap();
        let phone: SocketAddr = "192.168.1.50:5060".parse().unwrap();
        let t0 = Instant::now();
        relay.forward_invite(INVITE, phone, t0).unwrap();
        let branch = relay_branch(INVITE).unwrap();

        let ringing = response("180 Ringing", &branch);
        assert!(relay
            .response_for_phone(&ringing, account_addr, t0 + Duration::from_secs(170))
            .is_some());
        // 300 s after the INVITE: past any fixed 64 s window, within the
        // 180 s the last provisional bought.
        let answer = response("200 OK", &branch);
        let (to, forwarded) = relay
            .response_for_phone(&answer, account_addr, t0 + Duration::from_secs(300))
            .expect("late 200 must be delivered");
        assert_eq!(to, phone);
        assert_eq!(forwarded.matches("Via:").count(), 1, "relay Via stripped");
        // And once nothing is heard for the whole window, it is forgotten.
        assert!(relay
            .response_for_phone(&answer, account_addr, t0 + Duration::from_secs(900))
            .is_none());
    }

    #[test]
    fn a_response_from_anywhere_but_the_dial_out_account_is_dropped() {
        let account = UdpSocket::bind("127.0.0.1:0").unwrap();
        let account_addr = account.local_addr().unwrap();
        let relay = Relay::bind(
            "127.0.0.1",
            account_addr.port(),
            Arc::new(BindingStore::new()),
        )
        .unwrap();
        let t0 = Instant::now();
        relay
            .forward_invite(INVITE, "192.168.1.50:5060".parse().unwrap(), t0)
            .unwrap();
        let branch = relay_branch(INVITE).unwrap();
        let spoof: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(relay
            .response_for_phone(&response("200 OK", &branch), spoof, t0)
            .is_none());
    }

    #[test]
    fn an_ipv6_via_host_is_bracketed() {
        let out = rewrite_request(INVITE, "fd00::1".parse().unwrap(), 6000, "b").unwrap();
        assert!(out.contains("Via: SIP/2.0/UDP [fd00::1]:6000;branch=b"));
    }

    #[test]
    fn a_compact_via_header_is_recognised() {
        let compact = INVITE.replace("Via:", "v:");
        assert!(relay_branch(&compact).is_some());
        assert_eq!(relay_branch(&compact), relay_branch(INVITE));
    }

    #[test]
    fn the_relay_branch_is_stable_across_retransmission_and_cancel() {
        let cancel = INVITE.replace("INVITE", "CANCEL");
        assert_eq!(relay_branch(INVITE), relay_branch(&cancel));
        let other = INVITE.replace("c1", "c2");
        assert_ne!(relay_branch(INVITE), relay_branch(&other));
    }
}
