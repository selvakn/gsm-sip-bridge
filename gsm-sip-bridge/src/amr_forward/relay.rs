//! The socket forwarder for a forwarded call (FR-007, FR-008, FR-010, FR-013).
//!
//! Two UDP sockets, one per leg, and two pump threads between them. Packets
//! cross unchanged except for what the legs force: the audio and
//! telephone-event payload types are relabelled to what the receiving leg
//! negotiated, and the AMR packing is converted when the legs differ. SSRC,
//! sequence number and timestamp are never touched.
//!
//! Agent A uses it between its carrier socket and the veth hop to Agent B;
//! Agent B uses it between that hop and the client. It generalises the PCMU
//! `ims::agent::veth::relay_rtp` rather than copying it: the RTCP hooks and the
//! SSRC tracker are the same ones.

use super::repack::{repack, set_cmr, speech_modes};
use crate::ims::amr_rtp::AmrKind;
use crate::ims::rtcp::RelayRtcpRole;
use crate::ims::rtp::{parse_packet, SsrcTracker, RTP_HEADER_LEN};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How often a pump wakes to check `stop`.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Where a leg's packets are sent.
#[derive(Clone)]
pub enum Remote {
    /// The socket is `connect()`-ed to its peer.
    Connected,
    /// Re-learned during the call (a client's address or port changes on a
    /// re-INVITE, FR-011). Nothing is sent until it is set.
    Dynamic(Arc<Mutex<Option<SocketAddr>>>),
}

impl Remote {
    pub fn dynamic() -> (Self, Arc<Mutex<Option<SocketAddr>>>) {
        let cell = Arc::new(Mutex::new(None));
        (Remote::Dynamic(cell.clone()), cell)
    }
}

/// One side of the forwarder.
pub struct Leg {
    pub socket: UdpSocket,
    pub remote: Remote,
    /// The audio payload type this leg negotiated.
    pub payload_type: u8,
    /// Its `telephone-event` payload type, if it negotiated one.
    pub dtmf_payload_type: Option<u8>,
    pub octet_aligned: bool,
}

/// Marks "no telephone-event type" in [`LegIo::dtmf`].
const NO_DTMF: u16 = 0x100;

struct LegIo {
    socket: UdpSocket,
    remote: Remote,
    // Atomics, because a client's renegotiation can change what it expects and
    // the relay follows it (`ForwardSession::update_leg_b`).
    payload_type: AtomicU8,
    dtmf: AtomicU16,
    octet_aligned: AtomicBool,
}

impl LegIo {
    fn new(leg: Leg) -> Self {
        Self {
            socket: leg.socket,
            remote: leg.remote,
            payload_type: AtomicU8::new(leg.payload_type),
            dtmf: AtomicU16::new(leg.dtmf_payload_type.map_or(NO_DTMF, u16::from)),
            octet_aligned: AtomicBool::new(leg.octet_aligned),
        }
    }

    fn pt(&self) -> u8 {
        self.payload_type.load(Ordering::Relaxed)
    }

    fn dtmf_pt(&self) -> Option<u8> {
        u8::try_from(self.dtmf.load(Ordering::Relaxed)).ok()
    }

    fn oa(&self) -> bool {
        self.octet_aligned.load(Ordering::Relaxed)
    }

    /// Whether a datagram from `from` may enter the call. A connected socket
    /// only ever hears its peer; the client-facing socket is bound wide, so
    /// only the client's current address is accepted (anyone else on the
    /// network could otherwise inject audio or digits into the carrier call).
    fn accepts_from(&self, from: SocketAddr) -> bool {
        match &self.remote {
            Remote::Connected => true,
            Remote::Dynamic(cell) => *cell.lock().unwrap_or_else(|e| e.into_inner()) == Some(from),
        }
    }

    fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match &self.remote {
            Remote::Connected => self.socket.send(buf),
            Remote::Dynamic(cell) => {
                let to = *cell.lock().unwrap_or_else(|e| e.into_inner());
                match to {
                    Some(addr) => self.socket.send_to(buf, addr),
                    // Nothing learned yet: not an error, nothing to send to.
                    None => Ok(0),
                }
            }
        }
    }
}

/// Polices the carrier's `mode-set` when the client did not echo it
/// (`ModeSetPolicy::Relax*`). Used on the carrier-facing relay only: the
/// codec mode requests towards the client are clamped into the set, and the
/// client's speech frames outside it are counted and optionally dropped.
#[derive(Clone, Copy, Debug)]
pub struct ModeGuard {
    /// Bit `n` set = mode `n` is allowed on the carrier leg.
    pub modes: u16,
    pub drop: bool,
}

impl ModeGuard {
    fn highest(self) -> u8 {
        (15 - self.modes.leading_zeros()) as u8
    }

    fn allows(self, mode: u8) -> bool {
        mode < 16 && self.modes & (1 << mode) != 0
    }
}

/// Counters for [`ModeGuard`], shared by the two pumps and logged when they end.
#[derive(Default)]
struct GuardStats {
    cmr_clamped: AtomicU64,
    out_of_set: AtomicU64,
    frames_seen: AtomicU64,
}

/// Everything [`ForwardSession::start`] needs.
pub struct ForwardParams {
    pub kind: AmrKind,
    pub a: Leg,
    pub b: Leg,
    /// RTCP accounting for packets received from `a` / `b` (Agent A passes the
    /// carrier bundle's roles here; Agent B passes `None`).
    pub a_rtcp: Option<RelayRtcpRole>,
    pub b_rtcp: Option<RelayRtcpRole>,
    /// Bumped on every packet received from `a` / `b`, the same counters the
    /// one-way-audio monitor reads on a transcoded call (FR-013).
    pub a_rx: Arc<AtomicU64>,
    pub b_rx: Arc<AtomicU64>,
    /// Ends both pumps when set. A caller that already owns a per-call stop
    /// flag (Agent A's `ActiveCall`) passes it here and
    /// [`ForwardSession::detach`]es; others pass a fresh one and keep the
    /// session, whose `Drop` sets it.
    pub stop: Arc<AtomicBool>,
    /// See [`ModeGuard`]; `None` for Agent B's relay and strict calls.
    pub mode_guard: Option<ModeGuard>,
}

/// Whether packets may be sent *towards* each leg. A held or inactive leg is
/// closed (FR-010); its packets are still received and counted.
struct Gate {
    towards_a: AtomicBool,
    towards_b: AtomicBool,
}

/// A running forwarder. Dropping it stops the pumps, joins them and closes
/// both sockets, so every exit path of a call releases its ports (FR-017).
pub struct ForwardSession {
    stop: Arc<AtomicBool>,
    gate: Arc<Gate>,
    handles: Vec<JoinHandle<()>>,
    a_rx: Arc<AtomicU64>,
    b_rx: Arc<AtomicU64>,
    sent_to_a: Arc<AtomicU64>,
    sent_to_b: Arc<AtomicU64>,
    leg_b: Arc<LegIo>,
    detached: bool,
}

impl ForwardSession {
    pub fn start(p: ForwardParams) -> io::Result<Self> {
        for leg in [&p.a, &p.b] {
            leg.socket.set_read_timeout(Some(POLL_INTERVAL))?;
        }
        let kind = p.kind;
        let a = Arc::new(LegIo::new(p.a));
        let b = Arc::new(LegIo::new(p.b));
        let stop = p.stop;
        let gate = Arc::new(Gate {
            towards_a: AtomicBool::new(true),
            towards_b: AtomicBool::new(true),
        });
        let sent_to_a = Arc::new(AtomicU64::new(0));
        let sent_to_b = Arc::new(AtomicU64::new(0));
        let guard = p.mode_guard.map(|g| (g, Arc::new(GuardStats::default())));

        let pump_ab = Pump {
            kind,
            src: a.clone(),
            dst: b.clone(),
            stop: stop.clone(),
            gate: gate.clone(),
            open_if: |g: &Gate| g.towards_b.load(Ordering::Relaxed),
            received: p.a_rx.clone(),
            sent: sent_to_b.clone(),
            rtcp: p.a_rtcp,
            direction: "a->b",
            guard: guard.clone(),
        };
        let pump_ba = Pump {
            kind,
            src: b.clone(),
            dst: a,
            stop: stop.clone(),
            gate: gate.clone(),
            open_if: |g: &Gate| g.towards_a.load(Ordering::Relaxed),
            received: p.b_rx.clone(),
            sent: sent_to_a.clone(),
            rtcp: p.b_rtcp,
            direction: "b->a",
            guard,
        };
        let first = std::thread::Builder::new()
            .name("amr-fwd-ab".into())
            .spawn(move || pump_ab.run())?;
        let second = match std::thread::Builder::new()
            .name("amr-fwd-ba".into())
            .spawn(move || pump_ba.run())
        {
            Ok(h) => h,
            Err(e) => {
                // No session exists to stop the first pump: do it here, or it
                // would hold both sockets for good.
                stop.store(true, Ordering::Relaxed);
                let _ = first.join();
                return Err(e);
            }
        };
        let handles = vec![first, second];
        Ok(Self {
            stop,
            gate,
            handles,
            a_rx: p.a_rx,
            b_rx: p.b_rx,
            sent_to_a,
            sent_to_b,
            leg_b: b,
            detached: false,
        })
    }

    /// Let the pumps run on their own until the `stop` flag passed in
    /// [`ForwardParams`] is set. They release both sockets when they end.
    pub fn detach(mut self) {
        self.detached = true;
        self.handles.clear();
    }

    /// Open or close each direction (FR-010). `towards_a` governs packets sent
    /// to leg `a`, `towards_b` those sent to leg `b`.
    pub fn set_direction(&self, towards_a: bool, towards_b: bool) {
        self.gate.towards_a.store(towards_a, Ordering::Relaxed);
        self.gate.towards_b.store(towards_b, Ordering::Relaxed);
    }

    /// Follow the client's renegotiated payload types and packing (leg `b`).
    pub fn update_leg_b(
        &self,
        payload_type: u8,
        dtmf_payload_type: Option<u8>,
        octet_aligned: bool,
    ) {
        self.leg_b
            .payload_type
            .store(payload_type, Ordering::Relaxed);
        self.leg_b.dtmf.store(
            dtmf_payload_type.map_or(NO_DTMF, u16::from),
            Ordering::Relaxed,
        );
        self.leg_b
            .octet_aligned
            .store(octet_aligned, Ordering::Relaxed);
    }

    /// Packets received from `a`, from `b`.
    pub fn received(&self) -> (u64, u64) {
        (
            self.a_rx.load(Ordering::Relaxed),
            self.b_rx.load(Ordering::Relaxed),
        )
    }

    /// Packets actually sent to `a`, to `b`.
    pub fn sent(&self) -> (u64, u64) {
        (
            self.sent_to_a.load(Ordering::Relaxed),
            self.sent_to_b.load(Ordering::Relaxed),
        )
    }

    /// Stop the pumps and release both sockets. Same as dropping.
    pub fn stop(self) {}
}

impl Drop for ForwardSession {
    fn drop(&mut self) {
        if self.detached {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

struct Pump {
    kind: AmrKind,
    src: Arc<LegIo>,
    dst: Arc<LegIo>,
    stop: Arc<AtomicBool>,
    gate: Arc<Gate>,
    open_if: fn(&Gate) -> bool,
    received: Arc<AtomicU64>,
    sent: Arc<AtomicU64>,
    rtcp: Option<RelayRtcpRole>,
    direction: &'static str,
    guard: Option<(ModeGuard, Arc<GuardStats>)>,
}

impl Pump {
    /// Apply the mode guard to a packet before it is translated. `a->b` is
    /// the carrier's direction: its codec mode request is clamped into the
    /// set. `b->a` is the client's: its speech frames outside the set are
    /// counted and, if so configured, make the packet drop (`false`).
    fn apply_guard(&self, buf: &mut [u8]) -> bool {
        let Some((guard, stats)) = &self.guard else {
            return true;
        };
        let Some(pkt) = parse_packet(buf) else {
            return true;
        };
        if pkt.payload_type != self.src.pt() {
            return true;
        }
        let start = pkt.payload.as_ptr() as usize - buf.as_ptr() as usize;
        let Some((cmr, modes)) = speech_modes(pkt.payload, self.kind, self.src.oa()) else {
            return true;
        };
        if self.direction == "a->b" {
            if !guard.allows(cmr) {
                set_cmr(&mut buf[start..], guard.highest());
                crate::ims::media_stats::bump(&stats.cmr_clamped);
            }
            return true;
        }
        for mode in modes {
            crate::ims::media_stats::bump(&stats.frames_seen);
            if !guard.allows(mode) {
                if stats.out_of_set.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::warn!(
                        mode,
                        allowed = guard.modes,
                        drop = guard.drop,
                        "client sent a speech mode outside the carrier's mode-set"
                    );
                }
                if guard.drop {
                    return false;
                }
            }
        }
        true
    }

    fn log_guard_summary(&self) {
        if let Some((guard, stats)) = &self.guard {
            let out = stats.out_of_set.load(Ordering::Relaxed);
            let clamped = stats.cmr_clamped.load(Ordering::Relaxed);
            if self.direction == "b->a" || clamped > 0 {
                tracing::info!(
                    direction = self.direction,
                    allowed = guard.modes,
                    cmr_clamped = clamped,
                    frames_seen = stats.frames_seen.load(Ordering::Relaxed),
                    out_of_set = out,
                    dropped = if guard.drop { out } else { 0 },
                    "mode-set guard summary"
                );
            }
        }
    }

    fn run(self) {
        self.pump();
        self.log_guard_summary();
    }

    fn pump(&self) {
        let mut buf = [0u8; 2048];
        let mut ssrc_tracker = SsrcTracker::new();
        let media_start = Instant::now();
        let clock_rate = self.kind.clock_rate();
        while !self.stop.load(Ordering::Relaxed) {
            let n = match self.src.socket.recv_from(&mut buf) {
                Ok((n, from)) => {
                    if !self.src.accepts_from(from) {
                        continue;
                    }
                    n
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                // A connected UDP socket reports an ICMP port-unreachable from
                // a peer that is gone as ConnectionRefused; keep pumping, the
                // call's teardown decides when to stop.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                Err(_) => break,
            };
            crate::ims::media_stats::bump(&self.received);
            let Some(pkt) = parse_packet(&buf[..n]) else {
                continue;
            };
            if let Some((previous, new)) = ssrc_tracker.note_and_should_log(pkt.ssrc) {
                tracing::info!(
                    direction = self.direction,
                    previous_ssrc = previous,
                    new_ssrc = new,
                    "RTP source SSRC changed mid-call"
                );
            }
            crate::ims::rtcp::note_rtcp(&self.rtcp, &pkt, media_start.elapsed(), clock_rate);
            if !(self.open_if)(&self.gate) {
                continue;
            }
            if !self.apply_guard(&mut buf[..n]) {
                continue;
            }
            let Some(out) = self.translate(&mut buf[..n]) else {
                continue;
            };
            if self.dst.send(&out).is_ok_and(|sent| sent > 0) {
                crate::ims::media_stats::bump(&self.sent);
            }
        }
    }

    /// Relabel and, if the legs pack differently, repack. `None` drops the
    /// packet: an unknown payload type, or a payload the repacker cannot parse.
    fn translate<'b>(&self, buf: &'b mut [u8]) -> Option<std::borrow::Cow<'b, [u8]>> {
        let marker_and_pt = buf[1];
        let pt = marker_and_pt & 0x7f;
        if Some(pt) == self.src.dtmf_pt() {
            let dst_pt = self.dst.dtmf_pt()?;
            buf[1] = (marker_and_pt & 0x80) | (dst_pt & 0x7f);
            return Some(std::borrow::Cow::Borrowed(buf));
        }
        if pt != self.src.pt() {
            return None;
        }
        let (src_oa, dst_oa) = (self.src.oa(), self.dst.oa());
        if src_oa == dst_oa {
            buf[1] = (marker_and_pt & 0x80) | (self.dst.pt() & 0x7f);
            return Some(std::borrow::Cow::Borrowed(buf));
        }
        // Different packing: rebuild the datagram with a plain 12-byte header.
        // Reading the payload first ends the borrow of `buf`.
        let (seq_ts_ssrc, payload) = {
            let pkt = parse_packet(buf)?;
            let repacked = repack(pkt.payload, self.kind, src_oa, dst_oa)?;
            (buf[2..RTP_HEADER_LEN].to_vec(), repacked)
        };
        let mut out = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        out.push(0x80); // V=2, no padding, extension or CSRCs
        out.push((marker_and_pt & 0x80) | (self.dst.pt() & 0x7f));
        out.extend_from_slice(&seq_ts_ssrc);
        out.extend_from_slice(&payload);
        Some(std::borrow::Cow::Owned(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::rtp::build_packet;

    const PT_A: u8 = 104;
    const PT_B: u8 = 96;
    const EV_A: u8 = 105;
    const EV_B: u8 = 101;

    /// Both ends of both legs on loopback: the forwarder's sockets and the
    /// "carrier" / "client" test peers talking to them.
    struct Rig {
        session: ForwardSession,
        carrier: UdpSocket,
        client: UdpSocket,
        a_rx: Arc<AtomicU64>,
        b_rx: Arc<AtomicU64>,
        client_addr_cell: Arc<Mutex<Option<SocketAddr>>>,
        a_addr: SocketAddr,
        b_addr: SocketAddr,
    }

    fn rig(a_oa: bool, b_oa: bool) -> Rig {
        rig_guarded(a_oa, b_oa, None)
    }

    fn rig_guarded(a_oa: bool, b_oa: bool, mode_guard: Option<ModeGuard>) -> Rig {
        let carrier = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (a_addr, b_addr) = (a_sock.local_addr().unwrap(), b_sock.local_addr().unwrap());
        a_sock.connect(carrier.local_addr().unwrap()).unwrap();
        let (remote_b, cell) = Remote::dynamic();
        *cell.lock().unwrap() = Some(client.local_addr().unwrap());
        for s in [&carrier, &client] {
            s.set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
        }
        let a_rx = Arc::new(AtomicU64::new(0));
        let b_rx = Arc::new(AtomicU64::new(0));
        let session = ForwardSession::start(ForwardParams {
            kind: AmrKind::Wb,
            a: Leg {
                socket: a_sock,
                remote: Remote::Connected,
                payload_type: PT_A,
                dtmf_payload_type: Some(EV_A),
                octet_aligned: a_oa,
            },
            b: Leg {
                socket: b_sock,
                remote: remote_b,
                payload_type: PT_B,
                dtmf_payload_type: Some(EV_B),
                octet_aligned: b_oa,
            },
            a_rtcp: None,
            b_rtcp: None,
            a_rx: a_rx.clone(),
            b_rx: b_rx.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            mode_guard,
        })
        .unwrap();
        Rig {
            session,
            carrier,
            client,
            a_rx,
            b_rx,
            client_addr_cell: cell,
            a_addr,
            b_addr,
        }
    }

    /// An octet-aligned AMR-WB payload: CMR, ToC, 12.65k speech frame.
    fn oa_payload(cmr: u8, ft: u8, seed: u8) -> Vec<u8> {
        let bits: usize = match ft {
            2 => 253,
            8 => 477,
            9 => 40,
            15 => 0,
            _ => panic!("test frame type"),
        };
        let mut p = vec![cmr << 4, (ft << 3) | (1 << 2)];
        let mut data: Vec<u8> = (0..bits.div_ceil(8))
            .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
            .collect();
        if !bits.is_multiple_of(8) {
            let last = data.len() - 1;
            data[last] &= 0xffu8 << (8 - bits % 8);
        }
        p.extend(data);
        p
    }

    fn recv(sock: &UdpSocket) -> Vec<u8> {
        let mut buf = [0u8; 2048];
        let n = sock.recv(&mut buf).expect("a packet should have arrived");
        buf[..n].to_vec()
    }

    fn header_of(pkt: &[u8]) -> (u8, u8, &[u8]) {
        (pkt[0], pkt[1], &pkt[2..12])
    }

    #[test]
    fn payload_bytes_cross_unchanged_both_ways_with_only_the_payload_type_relabelled() {
        let r = rig(true, true);
        for (i, ft) in [2u8, 2, 9, 15, 2].into_iter().enumerate() {
            let payload = oa_payload(i as u8, ft, i as u8);
            // carrier -> client
            let sent = build_packet(
                100 + i as u16,
                16000 * i as u32,
                0xdead_beef,
                PT_A,
                &payload,
            );
            r.carrier.send_to(&sent, r.a_addr).unwrap();
            let got = recv(&r.client);
            assert_eq!(&got[12..], &payload[..], "payload identical");
            assert_eq!(got[1] & 0x7f, PT_B, "relabelled to the client's type");
            assert_eq!(
                header_of(&got).2,
                header_of(&sent).2,
                "seq, ts, ssrc untouched"
            );

            // client -> carrier
            let sent = build_packet(500 + i as u16, 320 * i as u32, 0x0bad_cafe, PT_B, &payload);
            r.client.send_to(&sent, r.b_addr).unwrap();
            let got = recv(&r.carrier);
            assert_eq!(&got[12..], &payload[..]);
            assert_eq!(got[1] & 0x7f, PT_A);
            assert_eq!(header_of(&got).2, header_of(&sent).2);
        }
    }

    const LOW_MODES: u16 = 0b1111;

    #[test]
    fn mode_guard_clamps_the_carriers_cmr_and_counts_but_forwards_by_default() {
        let r = rig_guarded(
            true,
            true,
            Some(ModeGuard {
                modes: LOW_MODES,
                drop: false,
            }),
        );
        // CMR 15 (no request) and 8 (outside the set) become 3; 1 is kept.
        for (cmr, want) in [(15u8, 3u8), (8, 3), (1, 1)] {
            let sent = build_packet(1, 0, 7, PT_A, &oa_payload(cmr, 2, 1));
            r.carrier.send_to(&sent, r.a_addr).unwrap();
            let got = recv(&r.client);
            assert_eq!(got[12] >> 4, want, "cmr {cmr}");
            assert_eq!(&got[13..], &oa_payload(cmr, 2, 1)[1..], "frame untouched");
        }
        // A mode 8 frame from the client still reaches the carrier.
        let p = oa_payload(15, 8, 3);
        r.client
            .send_to(&build_packet(2, 0, 8, PT_B, &p), r.b_addr)
            .unwrap();
        assert_eq!(&recv(&r.carrier)[12..], &p[..]);
    }

    #[test]
    fn mode_guard_drop_discards_only_out_of_set_speech_towards_the_carrier() {
        let r = rig_guarded(
            true,
            true,
            Some(ModeGuard {
                modes: LOW_MODES,
                drop: true,
            }),
        );
        let (bad, ok, sid) = (
            oa_payload(15, 8, 1),
            oa_payload(15, 2, 2),
            oa_payload(15, 9, 3),
        );
        for (seq, p) in [(1u16, &bad), (2, &ok), (3, &sid)] {
            r.client
                .send_to(&build_packet(seq, 0, 8, PT_B, p), r.b_addr)
                .unwrap();
        }
        // The first thing to arrive is the in-set frame, then the SID.
        assert_eq!(&recv(&r.carrier)[12..], &ok[..]);
        assert_eq!(&recv(&r.carrier)[12..], &sid[..]);
    }

    #[test]
    fn the_marker_bit_survives_relabelling() {
        let r = rig(true, true);
        let mut sent = build_packet(1, 0, 7, PT_A, &oa_payload(15, 2, 1));
        sent[1] |= 0x80;
        r.carrier.send_to(&sent, r.a_addr).unwrap();
        let got = recv(&r.client);
        assert_eq!(got[1], 0x80 | PT_B);
    }

    #[test]
    fn telephone_events_are_relabelled_per_leg() {
        let r = rig(true, true);
        let ev = [5u8, 0x8a, 0x00, 0xa0]; // digit 5, end, volume 10, duration 160
        r.carrier
            .send_to(&build_packet(1, 0, 7, EV_A, &ev), r.a_addr)
            .unwrap();
        let got = recv(&r.client);
        assert_eq!((got[1] & 0x7f, &got[12..]), (EV_B, &ev[..]));
        r.client
            .send_to(&build_packet(2, 0, 8, EV_B, &ev), r.b_addr)
            .unwrap();
        let got = recv(&r.carrier);
        assert_eq!((got[1] & 0x7f, &got[12..]), (EV_A, &ev[..]));
    }

    #[test]
    fn packing_is_converted_when_the_legs_differ_and_back_is_identical() {
        // carrier octet-aligned, client bandwidth-efficient
        let r = rig(true, false);
        for (ft, cmr) in [(2u8, 15u8), (2, 3), (9, 15), (15, 0)] {
            let oa = oa_payload(cmr, ft, 5);
            r.carrier
                .send_to(&build_packet(1, 0, 7, PT_A, &oa), r.a_addr)
                .unwrap();
            let be = recv(&r.client);
            assert_eq!(be[1] & 0x7f, PT_B);
            let expect = crate::amr_forward::repack::repack(&oa, AmrKind::Wb, true, false).unwrap();
            assert_eq!(&be[12..], &expect[..], "ft={ft}");
            assert_eq!(be[12] >> 4, cmr, "codec mode request kept");
            // and the client's answer comes back octet-aligned and identical
            r.client
                .send_to(&build_packet(2, 0, 8, PT_B, &be[12..]), r.b_addr)
                .unwrap();
            let back = recv(&r.carrier);
            assert_eq!(&back[12..], &oa[..], "a->b->a identical, ft={ft}");
        }
    }

    #[test]
    fn a_payload_that_cannot_be_repacked_is_dropped_not_forwarded() {
        let r = rig(true, false);
        // Reserved wideband frame type 11, octet-aligned.
        let garbled = vec![0xf0, 11 << 3, 0, 0];
        r.carrier
            .send_to(&build_packet(1, 0, 7, PT_A, &garbled), r.a_addr)
            .unwrap();
        r.client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let mut buf = [0u8; 64];
        assert!(
            r.client.recv(&mut buf).is_err(),
            "nothing should be forwarded"
        );
        // The forwarder carries on with the next good packet.
        r.carrier
            .send_to(
                &build_packet(2, 0, 7, PT_A, &oa_payload(15, 2, 1)),
                r.a_addr,
            )
            .unwrap();
        r.client
            .set_read_timeout(Some(Duration::from_millis(1000)))
            .unwrap();
        assert!(r.client.recv(&mut buf).is_ok());
    }

    #[test]
    fn an_unknown_payload_type_is_not_forwarded() {
        let r = rig(true, true);
        r.carrier
            .send_to(&build_packet(1, 0, 7, 33, &oa_payload(15, 2, 1)), r.a_addr)
            .unwrap();
        let mut buf = [0u8; 64];
        r.client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        assert!(r.client.recv(&mut buf).is_err());
    }

    #[test]
    fn rtp_from_anyone_but_the_client_is_not_forwarded() {
        let r = rig(true, true);
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        let payload = oa_payload(15, 2, 1);
        stranger
            .send_to(&build_packet(1, 0, 9, PT_B, &payload), r.b_addr)
            .unwrap();
        r.carrier
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        let mut buf = [0u8; 256];
        assert!(
            r.carrier.recv(&mut buf).is_err(),
            "a third party must not reach the carrier"
        );
        assert_eq!(
            r.b_rx.load(Ordering::Relaxed),
            0,
            "and is not counted as the client's audio"
        );
        // The real client still gets through afterwards.
        r.client
            .send_to(&build_packet(2, 0, 8, PT_B, &payload), r.b_addr)
            .unwrap();
        r.carrier
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert!(r.carrier.recv(&mut buf).is_ok());
    }

    #[test]
    fn the_client_leg_can_be_relabelled_mid_call() {
        let r = rig(true, true);
        r.session.update_leg_b(99, Some(98), true);
        let payload = oa_payload(15, 2, 1);
        r.carrier
            .send_to(&build_packet(1, 0, 7, PT_A, &payload), r.a_addr)
            .unwrap();
        assert_eq!(recv(&r.client)[1] & 0x7f, 99);
        r.client
            .send_to(&build_packet(2, 0, 8, 99, &payload), r.b_addr)
            .unwrap();
        assert_eq!(&recv(&r.carrier)[12..], &payload[..]);
    }

    #[test]
    fn a_held_leg_gets_nothing_and_resumes_when_reopened() {
        let r = rig(true, true);
        let payload = oa_payload(15, 2, 1);
        // Client on hold: nothing may be sent towards it (FR-010).
        r.session.set_direction(true, false);
        r.carrier
            .send_to(&build_packet(1, 0, 7, PT_A, &payload), r.a_addr)
            .unwrap();
        let mut buf = [0u8; 2048];
        r.client
            .set_read_timeout(Some(Duration::from_millis(400)))
            .unwrap();
        assert!(r.client.recv(&mut buf).is_err());
        // ...but it was received and counted, and the other direction flows.
        assert_eq!(r.a_rx.load(Ordering::Relaxed), 1);
        assert_eq!(r.session.sent().1, 0, "not counted as sent");
        r.client
            .send_to(&build_packet(2, 0, 8, PT_B, &payload), r.b_addr)
            .unwrap();
        assert_eq!(&recv(&r.carrier)[12..], &payload[..]);

        r.session.set_direction(true, true);
        r.carrier
            .send_to(&build_packet(3, 0, 7, PT_A, &payload), r.a_addr)
            .unwrap();
        r.client
            .set_read_timeout(Some(Duration::from_millis(1000)))
            .unwrap();
        assert_eq!(&recv(&r.client)[12..], &payload[..]);
    }

    #[test]
    fn a_new_client_address_takes_effect_for_the_next_packet() {
        let r = rig(true, true);
        let payload = oa_payload(15, 2, 1);
        let moved = UdpSocket::bind("127.0.0.1:0").unwrap();
        moved
            .set_read_timeout(Some(Duration::from_millis(1000)))
            .unwrap();
        *r.client_addr_cell.lock().unwrap() = Some(moved.local_addr().unwrap());
        r.carrier
            .send_to(&build_packet(1, 0, 7, PT_A, &payload), r.a_addr)
            .unwrap();
        assert_eq!(&recv(&moved)[12..], &payload[..]);
    }

    #[test]
    fn nothing_is_sent_to_a_client_whose_address_is_not_known_yet() {
        let r = rig(true, true);
        *r.client_addr_cell.lock().unwrap() = None;
        r.carrier
            .send_to(
                &build_packet(1, 0, 7, PT_A, &oa_payload(15, 2, 1)),
                r.a_addr,
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(r.session.sent(), (0, 0));
        assert_eq!(r.a_rx.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn counters_are_kept_per_direction() {
        let r = rig(true, true);
        let payload = oa_payload(15, 2, 1);
        for i in 0..3u16 {
            r.carrier
                .send_to(&build_packet(i, 0, 7, PT_A, &payload), r.a_addr)
                .unwrap();
            recv(&r.client);
        }
        r.client
            .send_to(&build_packet(9, 0, 8, PT_B, &payload), r.b_addr)
            .unwrap();
        recv(&r.carrier);
        assert_eq!(r.session.received(), (3, 1));
        assert_eq!(r.session.sent(), (1, 3));
        assert_eq!(
            (
                r.a_rx.load(Ordering::Relaxed),
                r.b_rx.load(Ordering::Relaxed)
            ),
            (3, 1)
        );
    }

    #[test]
    fn stopping_ends_the_threads_and_frees_the_ports() {
        let r = rig(true, true);
        let (a, b) = (r.a_addr, r.b_addr);
        let Rig { session, .. } = r;
        session.stop();
        // Both ports can be bound again.
        UdpSocket::bind(a).expect("leg a's port must be released");
        UdpSocket::bind(b).expect("leg b's port must be released");
    }

    #[test]
    fn a_detached_session_runs_until_the_shared_stop_flag_is_set() {
        let carrier = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let a_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (a_addr, b_addr) = (a_sock.local_addr().unwrap(), b_sock.local_addr().unwrap());
        a_sock.connect(carrier.local_addr().unwrap()).unwrap();
        b_sock.connect(client.local_addr().unwrap()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let session = ForwardSession::start(ForwardParams {
            kind: AmrKind::Wb,
            a: Leg {
                socket: a_sock,
                remote: Remote::Connected,
                payload_type: PT_A,
                dtmf_payload_type: None,
                octet_aligned: true,
            },
            b: Leg {
                socket: b_sock,
                remote: Remote::Connected,
                payload_type: PT_A,
                dtmf_payload_type: None,
                octet_aligned: true,
            },
            a_rtcp: None,
            b_rtcp: None,
            a_rx: Arc::new(AtomicU64::new(0)),
            b_rx: Arc::new(AtomicU64::new(0)),
            stop: stop.clone(),
            mode_guard: None,
        })
        .unwrap();
        session.detach(); // dropping the handle must not stop it
        let payload = oa_payload(15, 2, 1);
        carrier
            .send_to(&build_packet(1, 0, 7, PT_A, &payload), a_addr)
            .unwrap();
        assert_eq!(&recv(&client)[12..], &payload[..]);

        stop.store(true, Ordering::Relaxed);
        // The pumps notice within a poll interval and release their sockets.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if UdpSocket::bind(a_addr).is_ok() && UdpSocket::bind(b_addr).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "sockets were never released");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn dropping_the_session_is_the_same_as_stopping_it() {
        let r = rig(true, true);
        let (a, b) = (r.a_addr, r.b_addr);
        drop(r);
        UdpSocket::bind(a).unwrap();
        UdpSocket::bind(b).unwrap();
    }
}
