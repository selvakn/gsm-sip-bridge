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
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{MAX_DATAGRAM, READ_TIMEOUT};

/// How long a relayed transaction is remembered. RFC 3261 Timer B (64 × T1):
/// an INVITE transaction has no business outliving it.
const TRANSACTION_TTL: Duration = Duration::from_secs(64);

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
    expires: Instant,
}

pub(super) struct Relay {
    socket: UdpSocket,
    /// The dial-out account's address (`{host}:{local_port}`).
    target: SocketAddr,
    /// What the dial-out account sees as our source: the target's IP (we are
    /// on the same host) and the relay socket's own port.
    source: SocketAddr,
    /// The host written into our `Via` sent-by.
    via_host: String,
    transactions: Mutex<HashMap<String, Entry>>,
}

impl Relay {
    /// Binds the relay socket. `host` is the address the dial-out account is
    /// reachable on — the registrar's own listen address, or its realm when
    /// that is a wildcard (the same rule the `302` Contact uses).
    pub(super) fn bind(host: &str, port: u16, wildcard: bool) -> std::io::Result<Self> {
        let target = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                format!("no address for {host}"),
            )
        })?;
        // A specific listen address is bound exactly, so the dial-out account's
        // reply (and the Contact it advertises) resolves to the LAN address and
        // never 127.0.0.1. A wildcard listen address cannot be bound by name.
        let bind_ip = if wildcard { "0.0.0.0" } else { host };
        let socket = UdpSocket::bind((bind_ip, 0))?;
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        let local_port = socket.local_addr()?.port();
        Ok(Self {
            socket,
            target,
            source: SocketAddr::new(target.ip(), local_port),
            via_host: host.to_string(),
            transactions: Mutex::new(HashMap::new()),
        })
    }

    /// The source address the dial-out account sees relayed requests from.
    pub(super) fn source(&self) -> SocketAddr {
        self.source
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.transactions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Relays an INVITE from `peer`, remembering where its responses go.
    pub(super) fn forward_invite(
        &self,
        text: &str,
        peer: SocketAddr,
        now: Instant,
    ) -> Result<(), RelayError> {
        let branch = relay_branch(text).ok_or(RelayError::Malformed)?;
        let rewritten = rewrite_request(text, &self.via_host, self.source.port(), &branch)?;
        self.lock().insert(
            branch,
            Entry {
                peer,
                expires: now + TRANSACTION_TTL,
            },
        );
        self.socket
            .send_to(rewritten.as_bytes(), self.target)
            .map(|_| ())
            .map_err(|e| {
                tracing::warn!(error = %e, target = %self.target, "sip_server: relay to the dial-out account failed");
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
        let known = self
            .lock()
            .get(&branch)
            .is_some_and(|e| e.peer == peer && e.expires > now);
        if !known {
            return Ok(false);
        }
        let rewritten = rewrite_request(text, &self.via_host, self.source.port(), &branch)?;
        self.socket
            .send_to(rewritten.as_bytes(), self.target)
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
                    if from != self.target {
                        tracing::debug!(%from, "sip_server: relay ignoring a datagram not from the dial-out account");
                        continue;
                    }
                    if let Some((peer, response)) = self.response_for_phone(text, Instant::now()) {
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

    /// For a response from the dial-out account: the phone it belongs to and
    /// the response with our `Via` removed. `None` for anything that is not a
    /// response to a live relayed transaction.
    fn response_for_phone(&self, text: &str, now: Instant) -> Option<(SocketAddr, String)> {
        if !text.starts_with("SIP/2.0 ") {
            return None;
        }
        let mut lines: Vec<&str> = text.split("\r\n").collect();
        let via_idx = lines.iter().position(|l| header_is(l, "via"))?;
        let branch = param(lines[via_idx], "branch")?;
        let peer = {
            let map = self.lock();
            let entry = map.get(branch)?;
            if entry.expires <= now {
                return None;
            }
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
        if via_branch.is_none() && header_is(line, "via") {
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
    via_host: &str,
    via_port: u16,
    branch: &str,
) -> Result<String, RelayError> {
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
        let out = rewrite_request(INVITE, "10.0.0.1", 6000, "z9hG4bK-x").unwrap();
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
            rewrite_request(&text, "h", 1, "b"),
            Err(RelayError::TooManyHops)
        );
    }

    #[test]
    fn a_missing_max_forwards_gets_the_default() {
        let text = INVITE.replace("Max-Forwards: 70\r\n", "");
        let out = rewrite_request(&text, "h", 1, "b").unwrap();
        assert!(out.contains("Max-Forwards: 69\r\n"));
    }

    #[test]
    fn the_relay_branch_is_stable_across_retransmission_and_cancel() {
        let cancel = INVITE.replace("INVITE", "CANCEL");
        assert_eq!(relay_branch(INVITE), relay_branch(&cancel));
        let other = INVITE.replace("c1", "c2");
        assert_ne!(relay_branch(INVITE), relay_branch(&other));
    }
}
