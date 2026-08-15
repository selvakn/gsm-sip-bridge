//! The single unconnected UDP socket every SIP flow shares, plus the demux
//! reader thread that lets registration, outbound calls and inbound calls
//! all read from it safely.
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
//!
//! Exactly one thread ever calls `recv_from` on the socket — a background
//! reader spawned by [`SipSocket::bind`] — and demultiplexes each datagram
//! into one of two queues: `SipMessage::Response`s (consumed by
//! [`recv_response`](SipSocket::recv_response), used by registration and
//! outbound calling) and `SipMessage::Request`s (consumed by
//! [`recv_request`](SipSocket::recv_request), used by inbound-call handling).
//! Two threads independently calling `recv_from` on the same socket would
//! race for each datagram — whichever was blocked when it arrived gets it —
//! which could silently steal an inbound INVITE meant for the call handler,
//! or a response meant for an in-flight transaction. The single reader
//! removes that race entirely.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::sip_client::{parse_datagram, SipMessage, SipRequest, SipResponse};

use crate::error::SipTestResult;

const MAX_QUEUED: usize = 64;

struct Inbox {
    responses: Mutex<VecDeque<SipResponse>>,
    response_cond: Condvar,
    requests: Mutex<VecDeque<(SipRequest, SocketAddr)>>,
    request_cond: Condvar,
}

impl Default for Inbox {
    fn default() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            response_cond: Condvar::new(),
            requests: Mutex::new(VecDeque::new()),
            request_cond: Condvar::new(),
        }
    }
}

pub struct SipSocket {
    pub socket: Arc<UdpSocket>,
    pub local_ip: IpAddr,
    pub local_port: u16,
    inbox: Arc<Inbox>,
    reader_stop: Arc<AtomicBool>,
    reader_handle: Mutex<Option<thread::JoinHandle<()>>>,
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
        // `local_port == 0` requests an OS-assigned ephemeral port; read back
        // what was actually bound so `local_addr()` (used to build every
        // Via/Contact, and by tests to address this socket directly) is
        // correct rather than reporting port 0.
        let local_port = socket.local_addr()?.port();

        let local_ip = match local_ip {
            Some(ip) => ip,
            None => discover_routable_local_ip(probe_dst)?,
        };

        let socket = Arc::new(socket);
        let inbox = Arc::new(Inbox::default());
        let reader_stop = Arc::new(AtomicBool::new(false));

        let reader_handle = {
            let socket = socket.clone();
            let inbox = inbox.clone();
            let stop = reader_stop.clone();
            thread::spawn(move || reader_loop(socket, inbox, stop))
        };

        Ok(Self {
            socket,
            local_ip,
            local_port,
            inbox,
            reader_stop,
            reader_handle: Mutex::new(Some(reader_handle)),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::new(self.local_ip, self.local_port)
    }

    pub fn send(&self, dst: SocketAddr, msg: &str) -> SipTestResult<()> {
        self.socket.send_to(msg.as_bytes(), dst)?;
        Ok(())
    }

    /// Blocks until a SIP response arrives or `timeout` elapses.
    pub fn recv_response(&self, timeout: Duration) -> SipTestResult<Option<SipResponse>> {
        let deadline = Instant::now() + timeout;
        let mut guard = self
            .inbox
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(resp) = guard.pop_front() {
                return Ok(Some(resp));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (g, _timed_out) = self
                .inbox
                .response_cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
        }
    }

    /// Blocks until a SIP request (an inbound INVITE, BYE, CANCEL, OPTIONS...)
    /// arrives or `timeout` elapses, returning it with the peer address to
    /// reply to.
    pub fn recv_request(
        &self,
        timeout: Duration,
    ) -> SipTestResult<Option<(SipRequest, SocketAddr)>> {
        let deadline = Instant::now() + timeout;
        let mut guard = self
            .inbox
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(item) = guard.pop_front() {
                return Ok(Some(item));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (g, _timed_out) = self
                .inbox
                .request_cond
                .wait_timeout(guard, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
        }
    }
}

impl Drop for SipSocket {
    fn drop(&mut self) {
        self.reader_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self
            .reader_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = h.join();
        }
    }
}

fn reader_loop(socket: Arc<UdpSocket>, inbox: Arc<Inbox>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, src)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                match parse_datagram(&text) {
                    Ok(Some(SipMessage::Response(resp))) => {
                        let mut q = inbox.responses.lock().unwrap_or_else(|e| e.into_inner());
                        if q.len() >= MAX_QUEUED {
                            q.pop_front();
                        }
                        q.push_back(resp);
                        drop(q);
                        inbox.response_cond.notify_all();
                    }
                    Ok(Some(SipMessage::Request(req))) => {
                        let mut q = inbox.requests.lock().unwrap_or_else(|e| e.into_inner());
                        if q.len() >= MAX_QUEUED {
                            q.pop_front();
                        }
                        q.push_back((req, src));
                        drop(q);
                        inbox.request_cond.notify_all();
                    }
                    _ => {}
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => continue,
        }
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

    #[test]
    fn responses_and_requests_are_demultiplexed_into_separate_queues() {
        use gsm_sip_bridge::ims::sip_client::{build_options, OptionsRequest};

        let a = SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap();
        let b = SipSocket::bind(
            Some("127.0.0.1".parse().unwrap()),
            0,
            "127.0.0.1:1".parse().unwrap(),
        )
        .unwrap();

        // b sends a is a request (OPTIONS) — a should see it via recv_request.
        let options = build_options(&OptionsRequest {
            request_uri: "a@127.0.0.1",
            local_addr: b.local_addr(),
            transport: "UDP",
            public_uri: "sip:b@127.0.0.1",
            from_tag: "tag1",
            call_id: "call1",
            cseq: 1,
            branch: "z9hG4bKtest",
        });
        b.send(a.local_addr(), &options).unwrap();
        let (req, _src) = a
            .recv_request(Duration::from_secs(2))
            .unwrap()
            .expect("expected a to receive the OPTIONS request");
        assert_eq!(req.method, "OPTIONS");

        // a's response queue must still be empty — nothing crossed over.
        assert!(a
            .recv_response(Duration::from_millis(50))
            .unwrap()
            .is_none());

        // Now b sends a response; a should see it via recv_response, not recv_request.
        b.send(
            a.local_addr(),
            "SIP/2.0 200 OK\r\nCSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n",
        )
        .unwrap();
        let resp = a
            .recv_response(Duration::from_secs(2))
            .unwrap()
            .expect("expected a to receive the 200 OK");
        assert_eq!(resp.status, 200);
    }
}
