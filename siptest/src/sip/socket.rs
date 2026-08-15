//! The single unconnected UDP socket every SIP flow shares.
//!
//! `research.md` R2: a *connected* UDP socket carries a kernel-level source
//! filter, so it would silently discard the inbound INVITE the bridge sends
//! from its telephony agent's port (different from the registrar's port), and
//! it could not carry the post-302 re-INVITE to a different peer either. The
//! registrar also matches an outbound INVITE's source against the exact
//! `SocketAddr` that sent the REGISTER (`bindings.rs::find_by_source`), so a
//! fresh socket per transaction gets a `403` that looks like an auth fault.
//! One unconnected socket, `send_to`/`recv_from` only, is not a style
//! preference — it is the only correct model of this protocol.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::sip_client::{parse_datagram, SipMessage, SipResponse};

use crate::error::SipTestResult;

pub struct SipSocket {
    pub socket: Arc<UdpSocket>,
    pub local_ip: IpAddr,
    pub local_port: u16,
}

impl SipSocket {
    pub fn bind(
        local_ip: Option<IpAddr>,
        local_port: u16,
        probe_dst: SocketAddr,
    ) -> SipTestResult<Self> {
        let bind_addr: SocketAddr = match probe_dst {
            SocketAddr::V4(_) => format!("0.0.0.0:{local_port}").parse().unwrap(),
            SocketAddr::V6(_) => format!("[::]:{local_port}").parse().unwrap(),
        };
        let socket = UdpSocket::bind(bind_addr)?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;

        let local_ip = match local_ip {
            Some(ip) => ip,
            None => discover_routable_local_ip(probe_dst)?,
        };

        Ok(Self {
            socket: Arc::new(socket),
            local_ip,
            local_port,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::new(self.local_ip, self.local_port)
    }

    /// Blocks (in bounded 200ms slices, per the socket's read timeout) until
    /// a SIP *response* arrives or `timeout` elapses. Inbound *requests*
    /// arriving during the wait are dropped here — this MVP's registration
    /// and outbound-call transactions do not yet route through a shared
    /// dialog engine (that lands with inbound-call support), so a request
    /// interleaved with a response we're waiting on would otherwise be lost
    /// either way; better to drop it visibly than to misroute it.
    pub fn recv_response(&self, timeout: Duration) -> SipTestResult<Option<SipResponse>> {
        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; 4096];
        loop {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self.socket.recv_from(&mut buf) {
                Ok((n, _src)) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    if let Ok(Some(SipMessage::Response(resp))) = parse_datagram(&text) {
                        return Ok(Some(resp));
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn send(&self, dst: SocketAddr, msg: &str) -> SipTestResult<()> {
        self.socket.send_to(msg.as_bytes(), dst)?;
        Ok(())
    }
}

/// Discovers the local IP that would be used to reach `dst`, without sending
/// any traffic: bind an ephemeral UDP socket, `connect()` it (this performs a
/// route lookup but no handshake for UDP), read `local_addr()`, then drop it.
/// The one place in this crate that legitimately uses a connected socket,
/// because it is used only for the kernel's routing decision, never for I/O.
fn discover_routable_local_ip(dst: SocketAddr) -> SipTestResult<IpAddr> {
    let bind_addr: SocketAddr = match dst {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().unwrap(),
        SocketAddr::V6(_) => "[::]:0".parse().unwrap(),
    };
    let probe = UdpSocket::bind(bind_addr)?;
    probe.connect(dst)?;
    Ok(probe.local_addr()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_local_ip_is_never_unspecified_for_a_routable_destination() {
        // UDP `connect()` only performs a route lookup, no packet is sent, so
        // this needs no actual connectivity to the destination — any address
        // reachable via a default route works, including one that never
        // answers.
        let dst: SocketAddr = "8.8.8.8:53".parse().unwrap();
        let ip = discover_routable_local_ip(dst).unwrap();
        assert!(
            !ip.is_unspecified(),
            "discovered {ip} should be routable, not 0.0.0.0"
        );
    }
}
