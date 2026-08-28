//! RTCP (RFC 3550 §6) on the carrier-facing media leg of an answered call —
//! specs/046-rtcp-reporting, closing **RTP-01** (this bridge declared
//! `b=RS:`/`b=RR:` bandwidth in every SDP answer and sent or read no RTCP at
//! all) and SDP-06's deferred `a=rtcp` half. Deliberately minimal, matching
//! `sdp.rs`'s own posture: every session here is exactly two participants
//! (this bridge and the carrier), so RFC 3550 §6.3's multiparty scheduling
//! machinery (member counting, timer reconsideration, reverse
//! reconsideration on BYE) is not implemented — see
//! `specs/046-rtcp-reporting/research.md` Decision 9. RTCP extended reports
//! (XR), the AVPF feedback profile, and SRTCP are out of scope; a packet of
//! any such type is parsed only far enough to be recognised as `Unknown`
//! and skipped.
//!
//! Scope is the carrier leg of *answered* calls only (FR-023): the internal
//! veth leg to this project's own PJSIP and the originated-call path are
//! untouched, and continue to declare RTCP bandwidth they do not back
//! (FR-023a — a deliberate, recorded residue, not an oversight).

use crate::error::{BridgeError, BridgeResult};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------
// Wire format (RFC 3550 §6.4-6.6)
// ---------------------------------------------------------------------

const RTCP_VERSION: u8 = 2;
pub const PT_SR: u8 = 200;
pub const PT_RR: u8 = 201;
pub const PT_SDES: u8 = 202;
pub const PT_BYE: u8 = 203;
const SDES_CNAME: u8 = 1;
const SDES_END: u8 = 0;

/// One RFC 3550 §6.4.1 report block — what a sender or receiver report
/// states about one source it has been receiving.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportBlock {
    pub ssrc: u32,
    pub fraction_lost: u8,
    /// Signed: RFC 3550 allows this to go negative when duplicate packets
    /// outnumber genuine losses.
    pub cumulative_lost: i32,
    pub highest_seq: u32,
    pub jitter: u32,
    /// Middle 32 bits of the NTP timestamp from the last SR received from
    /// this source; zero if none has been received yet.
    pub lsr: u32,
    /// Delay, in the same units, since that SR was received; zero if `lsr`
    /// is zero.
    pub dlsr: u32,
}

fn rtcp_header(pt: u8, count: u8, body_len: usize) -> [u8; 4] {
    // RFC 3550 §6.4.1: length is this packet's size in 32-bit words minus
    // one, including the header itself.
    let length_words = ((4 + body_len) / 4).saturating_sub(1) as u16;
    let len_bytes = length_words.to_be_bytes();
    [
        (RTCP_VERSION << 6) | (count & 0x1f),
        pt,
        len_bytes[0],
        len_bytes[1],
    ]
}

fn push_report_block(buf: &mut Vec<u8>, b: &ReportBlock) {
    buf.extend_from_slice(&b.ssrc.to_be_bytes());
    let cumulative = (b.cumulative_lost as u32) & 0x00FF_FFFF;
    let word = ((b.fraction_lost as u32) << 24) | cumulative;
    buf.extend_from_slice(&word.to_be_bytes());
    buf.extend_from_slice(&b.highest_seq.to_be_bytes());
    buf.extend_from_slice(&b.jitter.to_be_bytes());
    buf.extend_from_slice(&b.lsr.to_be_bytes());
    buf.extend_from_slice(&b.dlsr.to_be_bytes());
}

fn parse_report_block(data: &[u8]) -> Option<ReportBlock> {
    if data.len() < 24 {
        return None;
    }
    let ssrc = u32::from_be_bytes(data[0..4].try_into().unwrap());
    let word = u32::from_be_bytes(data[4..8].try_into().unwrap());
    let fraction_lost = (word >> 24) as u8;
    let raw = word & 0x00FF_FFFF;
    // Sign-extend the 24-bit two's-complement value to i32.
    let cumulative_lost = if raw & 0x0080_0000 != 0 {
        (raw as i32) - 0x0100_0000
    } else {
        raw as i32
    };
    let highest_seq = u32::from_be_bytes(data[8..12].try_into().unwrap());
    let jitter = u32::from_be_bytes(data[12..16].try_into().unwrap());
    let lsr = u32::from_be_bytes(data[16..20].try_into().unwrap());
    let dlsr = u32::from_be_bytes(data[20..24].try_into().unwrap());
    Some(ReportBlock {
        ssrc,
        fraction_lost,
        cumulative_lost,
        highest_seq,
        jitter,
        lsr,
        dlsr,
    })
}

/// Builds a Sender Report (RFC 3550 §6.4.1). `ntp_timestamp` is the full
/// 64-bit NTP timestamp (32 bits seconds since 1900, 32 bits fraction) at
/// the moment `rtp_timestamp` was current on the media clock.
pub fn build_sender_report(
    ssrc: u32,
    ntp_timestamp: u64,
    rtp_timestamp: u32,
    packet_count: u32,
    octet_count: u32,
    report_block: Option<&ReportBlock>,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(24 + 24);
    body.extend_from_slice(&ssrc.to_be_bytes());
    body.extend_from_slice(&((ntp_timestamp >> 32) as u32).to_be_bytes());
    body.extend_from_slice(&(ntp_timestamp as u32).to_be_bytes());
    body.extend_from_slice(&rtp_timestamp.to_be_bytes());
    body.extend_from_slice(&packet_count.to_be_bytes());
    body.extend_from_slice(&octet_count.to_be_bytes());
    let rc = if let Some(b) = report_block {
        push_report_block(&mut body, b);
        1
    } else {
        0
    };
    let mut pkt = Vec::with_capacity(4 + body.len());
    pkt.extend_from_slice(&rtcp_header(PT_SR, rc, body.len()));
    pkt.extend_from_slice(&body);
    pkt
}

/// Builds a Receiver Report (RFC 3550 §6.4.2) — used when this leg has sent
/// no media of its own yet (FR-005/C-2.7), so there is nothing to describe
/// as a sender.
pub fn build_receiver_report(ssrc: u32, report_block: Option<&ReportBlock>) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + 24);
    body.extend_from_slice(&ssrc.to_be_bytes());
    let rc = if let Some(b) = report_block {
        push_report_block(&mut body, b);
        1
    } else {
        0
    };
    let mut pkt = Vec::with_capacity(4 + body.len());
    pkt.extend_from_slice(&rtcp_header(PT_RR, rc, body.len()));
    pkt.extend_from_slice(&body);
    pkt
}

/// Builds a Source Description (RFC 3550 §6.5) carrying a single CNAME
/// item, padded to a 32-bit boundary.
pub fn build_source_description(ssrc: u32, cname: &str) -> Vec<u8> {
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&ssrc.to_be_bytes());
    let cname_bytes = cname.as_bytes();
    let len = cname_bytes.len().min(255) as u8;
    chunk.push(SDES_CNAME);
    chunk.push(len);
    chunk.extend_from_slice(&cname_bytes[..len as usize]);
    chunk.push(SDES_END);
    while chunk.len() % 4 != 0 {
        chunk.push(0);
    }
    let mut pkt = Vec::with_capacity(4 + chunk.len());
    pkt.extend_from_slice(&rtcp_header(PT_SDES, 1, chunk.len()));
    pkt.extend_from_slice(&chunk);
    pkt
}

/// Builds a BYE (RFC 3550 §6.6) naming one source, with no reason string.
pub fn build_bye(ssrc: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(4);
    body.extend_from_slice(&ssrc.to_be_bytes());
    let mut pkt = Vec::with_capacity(4 + body.len());
    pkt.extend_from_slice(&rtcp_header(PT_BYE, 1, body.len()));
    pkt.extend_from_slice(&body);
    pkt
}

/// Concatenates already-built RTCP packets into one compound packet (RFC
/// 3550 §6.1: every compound packet must begin with an SR or RR, which is
/// the caller's responsibility to arrange).
pub fn compound(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// One member of a parsed compound RTCP packet. Deliberately not an
/// exhaustive model of RTCP — `Unknown` is the correct, final answer for
/// anything this bridge doesn't consume (APP, XR, and anything else),
/// mirroring `sdp.rs`'s own "the caller decides, we don't validate every
/// possibility" posture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtcpItem {
    SenderReport {
        ssrc: u32,
        ntp_timestamp: u64,
        rtp_timestamp: u32,
        packet_count: u32,
        octet_count: u32,
        blocks: Vec<ReportBlock>,
    },
    ReceiverReport {
        ssrc: u32,
        blocks: Vec<ReportBlock>,
    },
    SourceDescription {
        ssrc: u32,
        cname: Option<String>,
    },
    Bye {
        ssrcs: Vec<u32>,
    },
    Unknown {
        pt: u8,
    },
}

/// Parses a compound RTCP packet (RFC 3550 §6.1) into its members, walking
/// each one by its own length field. Never panics and never returns an
/// error type: a member whose declared length would run past the end of
/// `data` stops parsing there and returns whatever was already understood
/// (contract C-4.3/C-4.4) — the caller decides what "nothing understood"
/// means for its own diagnostics. An unrecognised payload type is skipped
/// via its own (still-trustworthy) length field, so it never aborts
/// parsing of the members after it.
pub fn parse_compound(data: &[u8]) -> Vec<RtcpItem> {
    let mut items = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let byte0 = data[offset];
        if byte0 >> 6 != RTCP_VERSION {
            break;
        }
        let count = (byte0 & 0x1f) as usize;
        let pt = data[offset + 1];
        let length_words = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let packet_len = (length_words + 1) * 4;
        if offset + packet_len > data.len() {
            break;
        }
        let body = &data[offset + 4..offset + packet_len];
        match pt {
            PT_SR => {
                if body.len() < 24 {
                    break;
                }
                let ssrc = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let ntp_msw = u32::from_be_bytes(body[4..8].try_into().unwrap());
                let ntp_lsw = u32::from_be_bytes(body[8..12].try_into().unwrap());
                let rtp_timestamp = u32::from_be_bytes(body[12..16].try_into().unwrap());
                let packet_count = u32::from_be_bytes(body[16..20].try_into().unwrap());
                let octet_count = u32::from_be_bytes(body[20..24].try_into().unwrap());
                let mut blocks = Vec::new();
                let mut b_off = 24;
                for _ in 0..count {
                    match body.get(b_off..).and_then(parse_report_block) {
                        Some(b) => {
                            blocks.push(b);
                            b_off += 24;
                        }
                        None => break,
                    }
                }
                items.push(RtcpItem::SenderReport {
                    ssrc,
                    ntp_timestamp: ((ntp_msw as u64) << 32) | ntp_lsw as u64,
                    rtp_timestamp,
                    packet_count,
                    octet_count,
                    blocks,
                });
            }
            PT_RR => {
                if body.len() < 4 {
                    break;
                }
                let ssrc = u32::from_be_bytes(body[0..4].try_into().unwrap());
                let mut blocks = Vec::new();
                let mut b_off = 4;
                for _ in 0..count {
                    match body.get(b_off..).and_then(parse_report_block) {
                        Some(b) => {
                            blocks.push(b);
                            b_off += 24;
                        }
                        None => break,
                    }
                }
                items.push(RtcpItem::ReceiverReport { ssrc, blocks });
            }
            PT_SDES => {
                let mut b_off = 0usize;
                for _ in 0..count {
                    if b_off + 4 > body.len() {
                        break;
                    }
                    let ssrc = u32::from_be_bytes(body[b_off..b_off + 4].try_into().unwrap());
                    let mut item_off = b_off + 4;
                    let mut cname = None;
                    loop {
                        if item_off >= body.len() {
                            break;
                        }
                        let item_type = body[item_off];
                        if item_type == SDES_END {
                            item_off += 1;
                            break;
                        }
                        if item_off + 1 >= body.len() {
                            break;
                        }
                        let item_len = body[item_off + 1] as usize;
                        let text_start = item_off + 2;
                        let text_end = text_start + item_len;
                        if text_end > body.len() {
                            break;
                        }
                        if item_type == SDES_CNAME {
                            cname = Some(
                                String::from_utf8_lossy(&body[text_start..text_end]).into_owned(),
                            );
                        }
                        item_off = text_end;
                    }
                    items.push(RtcpItem::SourceDescription { ssrc, cname });
                    // Next chunk starts on a 32-bit boundary relative to the
                    // start of the SDES body (itself 32-bit aligned).
                    b_off = (item_off + 3) & !3;
                }
            }
            PT_BYE => {
                let mut ssrcs = Vec::new();
                let mut b_off = 0usize;
                for _ in 0..count {
                    if b_off + 4 > body.len() {
                        break;
                    }
                    ssrcs.push(u32::from_be_bytes(
                        body[b_off..b_off + 4].try_into().unwrap(),
                    ));
                    b_off += 4;
                }
                items.push(RtcpItem::Bye { ssrcs });
            }
            other => {
                items.push(RtcpItem::Unknown { pt: other });
            }
        }
        offset += packet_len;
    }
    items
}

/// Derives round-trip time from a receiver block's LSR/DLSR per RFC 3550
/// §6.4.1: `RTT = now - LSR - DLSR`, all in the NTP "middle 32 bits" units
/// (Q16.16 seconds). `lsr == 0` means the far end has not yet acknowledged
/// any SR from us, which is not a round trip of zero (contract C-5.2) —
/// the case a naive implementation gets wrong.
pub fn derive_round_trip(now_ntp_mid: u32, lsr: u32, dlsr: u32) -> Option<Duration> {
    if lsr == 0 {
        return None;
    }
    let rtt_units = now_ntp_mid.wrapping_sub(lsr).wrapping_sub(dlsr);
    // A genuinely negative result (clock skew, a malformed DLSR) wraps to a
    // huge u32 close to u32::MAX; treat anything implausibly large as "not
    // derivable" rather than reporting a nonsensical multi-year RTT.
    if rtt_units > (1u32 << 31) {
        return None;
    }
    let seconds = (rtt_units >> 16) as u64;
    let frac = (rtt_units & 0xFFFF) as f64 / 65536.0;
    Some(Duration::from_secs(seconds) + Duration::from_secs_f64(frac))
}

/// The NTP "middle 32 bits" of a full 64-bit NTP timestamp — the form RFC
/// 3550 §6.4.1 uses for LSR and for `derive_round_trip`'s `now` argument.
pub fn ntp_middle_32(ntp_timestamp: u64) -> u32 {
    (ntp_timestamp >> 16) as u32
}

// ---------------------------------------------------------------------
// Report cadence (FR-004/004a/004b — research.md Decision 9)
// ---------------------------------------------------------------------

/// When the next compound RTCP packet is due, derived from the declared
/// sender bandwidth (`b=RS:` — `sdp.rs`) and the average size of what is
/// actually sent, then randomised within ±50% of that value (RFC 3550
/// §6.3.1) so independent participants can't fall into lockstep. Carries
/// no member count and no timer reconsideration (FR-004b) — every session
/// here is two-party by construction.
pub struct ReportSchedule {
    bandwidth_bps: u32,
    mean_packet_bytes: f64,
    samples: u32,
    next_due: Instant,
}

impl ReportSchedule {
    /// A reasonable prior for the mean compound packet size before the
    /// first one is actually sent (roughly an SR+SDES with one report
    /// block) — refined immediately once `record_packet_size` is called.
    const INITIAL_MEAN_BYTES: f64 = 60.0;

    pub fn new(bandwidth_bps: u32, now: Instant) -> Self {
        let mut s = Self {
            bandwidth_bps: bandwidth_bps.max(1),
            mean_packet_bytes: Self::INITIAL_MEAN_BYTES,
            samples: 0,
            next_due: now,
        };
        s.reschedule(now);
        s
    }

    fn base_interval(&self) -> Duration {
        let bits_per_packet = self.mean_packet_bytes * 8.0;
        Duration::from_secs_f64((bits_per_packet / self.bandwidth_bps as f64).max(0.001))
    }

    fn reschedule(&mut self, now: Instant) {
        // RFC 3550 §6.3.1: randomise within [0.5, 1.5] x the base interval.
        let factor = 0.5 + rand::random::<f64>();
        self.next_due = now + self.base_interval().mul_f64(factor);
    }

    /// Folds one more sent packet's size into the running mean used to
    /// derive the base interval.
    pub fn record_packet_size(&mut self, bytes: usize) {
        self.samples += 1;
        let n = self.samples as f64;
        self.mean_packet_bytes += (bytes as f64 - self.mean_packet_bytes) / n;
    }

    /// True at most once per elapsed interval; rolls a freshly randomised
    /// deadline forward as a side effect of returning `true`.
    pub fn is_due(&mut self, now: Instant) -> bool {
        if now >= self.next_due {
            self.reschedule(now);
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub fn next_due(&self) -> Instant {
        self.next_due
    }
}

// ---------------------------------------------------------------------
// Source trust (FR-010a/010b — research.md Decision 7)
// ---------------------------------------------------------------------

/// How often one call's RTCP source-address rejection can be logged, at
/// most — mirroring `rtp::SsrcTracker`'s own rate limit (specs/044 RTP-04)
/// for the identical reason: a misdirected or hostile sender must not be
/// able to turn one diagnostic into a log line per packet.
const REJECTION_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Guards inbound RTCP against anything not from the call's negotiated
/// peer. Checks the **address only**, deliberately not the port
/// (research.md Decision 7): a peer sending RTCP from a source port other
/// than the one it receives on is unusual but not wrong, and rejecting on
/// port would silently discard a legitimate report — reintroducing, at a
/// different layer, the "we see silence and cannot tell why" problem this
/// feature exists to end.
pub struct SourceGuard {
    peer_ip: IpAddr,
    last_rejected_logged_at: Option<Instant>,
    rejected_count: u64,
}

impl SourceGuard {
    pub fn new(peer_ip: IpAddr) -> Self {
        Self {
            peer_ip,
            last_rejected_logged_at: None,
            rejected_count: 0,
        }
    }

    /// Whether RTCP claiming to arrive `from` should be trusted. Does
    /// **not** additionally require a known SSRC (FR-010b) — a report
    /// naming a source the bridge has only just started seeing is exactly
    /// what a legitimate report looks like right after a mid-call source
    /// restart.
    pub fn accept(&mut self, from: SocketAddr) -> bool {
        if from.ip() == self.peer_ip {
            return true;
        }
        self.rejected_count += 1;
        let now = Instant::now();
        let should_log = match self.last_rejected_logged_at {
            Some(at) => now.duration_since(at) >= REJECTION_LOG_INTERVAL,
            None => true,
        };
        if should_log {
            self.last_rejected_logged_at = Some(now);
            tracing::warn!(
                expected = %self.peer_ip,
                from = %from,
                "discarding RTCP from an unexpected source address"
            );
        }
        false
    }

    #[cfg(test)]
    pub fn rejected_count(&self) -> u64 {
        self.rejected_count
    }
}

// ---------------------------------------------------------------------
// Ports (FR-014/015/016/017 — research.md Decision 1)
// ---------------------------------------------------------------------

/// What binding RTP + RTCP for one call produced. `rtp_socket` always
/// exists — media must proceed regardless of what happened to RTCP
/// (SC-006). `rtcp` is `None` only on tier 3 (no RTCP port obtainable at
/// all).
pub struct RtpRtcpBind {
    pub rtp_socket: UdpSocket,
    pub rtp_port: u16,
    /// `Some((socket, port, declared))`. `declared` is true only for tier
    /// 2 — an ephemeral, non-conventional port — meaning the SDP answer
    /// must state `a=rtcp:<port>` (RFC 3605). Tier 1's port is the RFC
    /// 3550 §11 default (RTP+1) and needs no attribute; the answer stays
    /// byte-identical to today's (contract C-1.1).
    pub rtcp: Option<(UdpSocket, u16, bool)>,
}

const MAX_BIND_ATTEMPTS: u32 = 10;

/// Binds an RTP socket and, if possible, its conventional RTCP companion
/// (RTP port + 1) on `ip`, retrying the pair up to a bounded number of
/// times when the RTP port lands odd or its neighbour is already taken.
/// Falls back to declaring any ephemeral RTCP port, and finally to no RTCP
/// at all — see the module doc and research.md Decision 1. Only errors if
/// even the final RTP bind attempt fails, exactly like the plain
/// `UdpSocket::bind` this replaces at the call site.
pub fn bind_rtp_and_rtcp(ip: IpAddr) -> BridgeResult<RtpRtcpBind> {
    let mut last: Option<(UdpSocket, u16)> = None;
    for _ in 0..MAX_BIND_ATTEMPTS {
        let rtp_socket = UdpSocket::bind((ip, 0))
            .map_err(|e| BridgeError::Ims(format!("RTP socket bind failed: {e}")))?;
        let rtp_port = rtp_socket
            .local_addr()
            .map_err(|e| BridgeError::Ims(format!("RTP local_addr failed: {e}")))?
            .port();
        if rtp_port % 2 != 0 {
            last = Some((rtp_socket, rtp_port));
            continue;
        }
        match UdpSocket::bind((ip, rtp_port + 1)) {
            Ok(rtcp_socket) => {
                return Ok(RtpRtcpBind {
                    rtp_socket,
                    rtp_port,
                    rtcp: Some((rtcp_socket, rtp_port + 1, false)),
                });
            }
            Err(_) => {
                last = Some((rtp_socket, rtp_port));
                continue;
            }
        }
    }

    let (rtp_socket, rtp_port) = match last {
        Some(pair) => pair,
        None => {
            let s = UdpSocket::bind((ip, 0))
                .map_err(|e| BridgeError::Ims(format!("RTP socket bind failed: {e}")))?;
            let p = s
                .local_addr()
                .map_err(|e| BridgeError::Ims(format!("RTP local_addr failed: {e}")))?
                .port();
            (s, p)
        }
    };
    match UdpSocket::bind((ip, 0)) {
        Ok(rtcp_socket) => {
            let rtcp_port = rtcp_socket
                .local_addr()
                .map_err(|e| BridgeError::Ims(format!("RTCP local_addr failed: {e}")))?
                .port();
            Ok(RtpRtcpBind {
                rtp_socket,
                rtp_port,
                rtcp: Some((rtcp_socket, rtcp_port, true)),
            })
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "no RTCP port obtainable for this call; proceeding without RTCP"
            );
            Ok(RtpRtcpBind {
                rtp_socket,
                rtp_port,
                rtcp: None,
            })
        }
    }
}

// ---------------------------------------------------------------------
// Far-end quality (FR-006/007/008/009 — US2)
// ---------------------------------------------------------------------

/// What the far end has told us about what it received from us. Every
/// field starts empty and stays empty until a well-formed report supplies
/// it — never defaulted to zero, so "never reported" stays distinguishable
/// from "reported zero loss" (FR-009).
#[derive(Debug, Clone, Copy, Default)]
pub struct FarEndQuality {
    pub reports_received: u64,
    pub fraction_lost: Option<u8>,
    pub cumulative_lost: Option<i32>,
    pub jitter: Option<Duration>,
    pub round_trip: Option<Duration>,
}

impl FarEndQuality {
    /// Records one receiver block from the far end. `our_last_sr` is the
    /// NTP timestamp (and when it was sent) of the most recent SR *this*
    /// bridge sent, if any — needed to derive round-trip time; `None`
    /// leaves `round_trip` untouched rather than deriving a bogus value.
    pub fn record(&mut self, block: &ReportBlock, clock_rate: u32, now_ntp: u64) {
        self.reports_received += 1;
        self.fraction_lost = Some(block.fraction_lost);
        self.cumulative_lost = Some(block.cumulative_lost);
        if clock_rate > 0 {
            self.jitter = Some(Duration::from_secs_f64(
                block.jitter as f64 / clock_rate as f64,
            ));
        }
        self.round_trip = derive_round_trip(ntp_middle_32(now_ntp), block.lsr, block.dlsr);
    }
}

// ---------------------------------------------------------------------
// Relay wiring (US1/US3 — research.md Decision 10)
// ---------------------------------------------------------------------

/// What one relay direction should do for RTCP, if anything. At most one
/// variant applies per direction — a relay leg either receives the
/// carrier's media or sends toward the carrier, never both — bundled as a
/// single new parameter on each relay signature (research.md Decision 10)
/// rather than adding several to functions that already carry
/// `#[allow(clippy::too_many_arguments)]`.
#[derive(Clone)]
pub enum RelayRtcpRole {
    /// This direction sends toward the carrier: publish what actually
    /// leaves on that stream (FR-002b).
    Sender(crate::ims::media_stats::SendAccounting),
    /// This direction receives from the carrier: measure receive quality
    /// (FR-011) and publish the carrier's current SSRC, needed for the
    /// receiver block this bridge includes in its own outgoing SR/RR
    /// (contract C-2.6).
    Receiver {
        tracker: Arc<Mutex<crate::ims::media_stats::ReceiveTracker>>,
        ssrc: Arc<Mutex<Option<u32>>>,
    },
}

/// The shared state behind both directions' [`RelayRtcpRole`]s for one
/// call's carrier leg, and what the per-call RTCP thread itself reads
/// from and writes to.
#[derive(Clone, Default)]
pub struct CarrierRtcpBundle {
    pub send: crate::ims::media_stats::SendAccounting,
    pub receive: Arc<Mutex<crate::ims::media_stats::ReceiveTracker>>,
    pub receive_ssrc: Arc<Mutex<Option<u32>>>,
    pub far_end: Arc<Mutex<FarEndQuality>>,
}

impl CarrierRtcpBundle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sender_role(&self) -> RelayRtcpRole {
        RelayRtcpRole::Sender(self.send.clone())
    }

    pub fn receiver_role(&self) -> RelayRtcpRole {
        RelayRtcpRole::Receiver {
            tracker: self.receive.clone(),
            ssrc: self.receive_ssrc.clone(),
        }
    }
}

// ---------------------------------------------------------------------
// The per-call RTCP thread (US1/US2/US5)
// ---------------------------------------------------------------------

/// How often the report loop's blocking `recv_from` wakes up to check
/// `stop` — matches the relay threads' own `RELAY_POLL_INTERVAL`
/// (`agent::veth`/`transcode`), so a hangup is noticed on the same
/// timescale everywhere in the media path.
const REPORT_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Difference between the NTP epoch (1900-01-01) and the Unix epoch
/// (1970-01-01), in seconds — RFC 5905 Appendix A.
const NTP_UNIX_EPOCH_DIFF_SECS: u64 = 2_208_988_800;

/// The current time as a 64-bit NTP timestamp (32 bits seconds since 1900,
/// 32 bits fraction) — this bridge has no NTP discipline of its own, so
/// this is wall-clock time, exactly as accurate as the host's clock is.
fn ntp_now() -> u64 {
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_unix.as_secs() + NTP_UNIX_EPOCH_DIFF_SECS;
    let frac = ((since_unix.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// Spawns the per-call RTCP thread: sends periodic compound reports on
/// `schedule`'s cadence, reads what the far end sends back into
/// `bundle.far_end`, and sends a BYE (research.md Decision 2 — no
/// teardown call site needs to change for this) once `stop` is observed.
/// `clock_rate` is the *carrier* codec's clock rate — the receiver block
/// this bridge includes in its own reports describes what it received
/// from the carrier, so it ticks on the carrier's clock, not the veth
/// leg's.
pub fn spawn_report_loop(
    socket: UdpSocket,
    remote: SocketAddr,
    peer_ip: IpAddr,
    bundle: CarrierRtcpBundle,
    clock_rate: u32,
    cname: String,
    stop: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        run_report_loop(socket, remote, peer_ip, bundle, clock_rate, cname, stop)
    });
}

fn run_report_loop(
    socket: UdpSocket,
    remote: SocketAddr,
    peer_ip: IpAddr,
    bundle: CarrierRtcpBundle,
    clock_rate: u32,
    cname: String,
    stop: Arc<AtomicBool>,
) {
    if socket.set_read_timeout(Some(REPORT_POLL_INTERVAL)).is_err() {
        tracing::warn!("RTCP socket set_read_timeout failed; polling may block past a hangup");
    }
    let mut guard = SourceGuard::new(peer_ip);
    let mut schedule = ReportSchedule::new(super::sdp::RTCP_SR_BANDWIDTH_BPS, Instant::now());
    let mut buf = [0u8; 2048];

    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if guard.accept(from) {
                    for item in parse_compound(&buf[..n]) {
                        handle_inbound_item(&item, &bundle, clock_rate);
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => {
                tracing::warn!(error = %e, "RTCP recv failed");
            }
        }

        let now = Instant::now();
        if schedule.is_due(now) {
            let pkt = build_report(&bundle, clock_rate, &cname);
            schedule.record_packet_size(pkt.len());
            if let Err(e) = socket.send_to(&pkt, remote) {
                tracing::warn!(error = %e, "RTCP send failed");
            }
        }
    }

    // FR-018: a leaving-source indication before the socket closes — no
    // reason to send one if we never established a source at all.
    if let Some(snap) = bundle.send.snapshot() {
        if let Err(e) = socket.send_to(&build_bye(snap.ssrc), remote) {
            // FR-019: never propagated, never blocks — the socket is
            // about to be dropped either way.
            tracing::warn!(error = %e, "RTCP BYE send failed");
        }
    }
}

/// Routes report blocks addressed to *our own* reported SSRC into
/// `bundle.far_end` — regardless of whether the far end wrapped them in a
/// Sender Report (it is also transmitting audio to us, the ordinary case
/// on a live two-way call) or a Receiver Report (it has sent us nothing).
/// Anything else (a report block about some other source, or an RTCP type
/// this bridge doesn't consume) is ignored, not an error.
fn handle_inbound_item(item: &RtcpItem, bundle: &CarrierRtcpBundle, clock_rate: u32) {
    let Some(our_ssrc) = bundle.send.snapshot().map(|s| s.ssrc) else {
        // We haven't sent anything yet, so nothing the far end reports can
        // be about us.
        return;
    };
    let blocks: &[ReportBlock] = match item {
        RtcpItem::SenderReport { blocks, .. } => blocks,
        RtcpItem::ReceiverReport { blocks, .. } => blocks,
        _ => return,
    };
    for b in blocks {
        if b.ssrc == our_ssrc {
            if let Ok(mut fe) = bundle.far_end.lock() {
                fe.record(b, clock_rate, ntp_now());
            }
        }
    }
}

/// Builds one compound RTCP packet describing this call's carrier leg:
/// a Sender Report when this side has sent anything, otherwise a Receiver
/// Report (contract C-2.7); a receiver block describing what was received
/// from the carrier, when there is a source to describe it against; and a
/// CNAME.
///
/// The receiver block's `lsr`/`dlsr` are left `0` — meaning "no SR from
/// the far end has been correlated yet" (a legal RFC 3550 value,
/// `derive_round_trip` already treats it that way) — tracking the far
/// end's own SR arrival time to fill them precisely is out of scope here;
/// this feature's round-trip measurement (FR-007) is the far end's report
/// about *us*, read in `handle_inbound_item`, not this bridge's own report
/// about the far end. `fraction_lost` is likewise a simplification: the
/// *cumulative* loss fraction rather than the fraction since the previous
/// report RFC 3550 §6.4.1 strictly defines — an honest approximation
/// documented here rather than a fabricated precise figure.
fn build_report(bundle: &CarrierRtcpBundle, clock_rate: u32, cname: &str) -> Vec<u8> {
    let receive_block = bundle
        .receive_ssrc
        .lock()
        .ok()
        .and_then(|g| *g)
        .zip(
            bundle
                .receive
                .lock()
                .ok()
                .map(|t| (t.stats(clock_rate), t.highest_extended_seq().unwrap_or(0))),
        )
        .map(|(ssrc, (stats, highest_seq))| ReportBlock {
            ssrc,
            fraction_lost: ((stats.loss_percent() / 100.0).clamp(0.0, 1.0) * 255.0) as u8,
            cumulative_lost: stats.lost_packets as i32,
            highest_seq,
            jitter: (stats.jitter.as_secs_f64() * clock_rate as f64).round() as u32,
            lsr: 0,
            dlsr: 0,
        });

    let sr_sr;
    let rr_sr;
    let primary: &[u8] = match bundle.send.snapshot() {
        Some(snap) => {
            let ntp = ntp_now();
            sr_sr = build_sender_report(
                snap.ssrc,
                ntp,
                snap.last_rtp_timestamp,
                snap.packets as u32,
                snap.octets as u32,
                receive_block.as_ref(),
            );
            &sr_sr
        }
        None => {
            // Nothing sent yet — a receiver report needs *some* SSRC to
            // report under; without one (no send, and nothing received
            // from the carrier either) there is nothing coherent to send.
            let Some(rb) = &receive_block else {
                return Vec::new();
            };
            rr_sr = build_receiver_report(rb.ssrc, receive_block.as_ref());
            &rr_sr
        }
    };
    let sdes = build_source_description(
        // SDES names the same source the SR/RR above just did.
        match bundle.send.snapshot() {
            Some(snap) => snap.ssrc,
            None => receive_block.as_ref().map(|b| b.ssrc).unwrap_or(0),
        },
        cname,
    );
    compound(&[primary, &sdes])
}

// ---------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};

    fn block(ssrc: u32) -> ReportBlock {
        ReportBlock {
            ssrc,
            fraction_lost: 5,
            cumulative_lost: 3,
            highest_seq: 1000,
            jitter: 42,
            lsr: 0x0001_0000,
            dlsr: 0x0000_8000,
        }
    }

    // ---- building --------------------------------------------------

    #[test]
    fn a_sender_report_with_no_block_states_rc_zero_and_omits_it() {
        let pkt = build_sender_report(0xAAAA_BBBB, 0, 0, 0, 0, None);
        assert_eq!(pkt.len(), 28, "header(4) + sender info(24)");
        assert_eq!(pkt[0] & 0x1f, 0, "RC must be 0 with no report block");
        assert_eq!(pkt[1], PT_SR);
    }

    #[test]
    fn a_sender_report_with_a_block_round_trips_through_parsing() {
        let b = block(0x1234_5678);
        let pkt = build_sender_report(0xAAAA_BBBB, 0x1122_3344_5566_7788, 999, 10, 2000, Some(&b));
        let items = parse_compound(&pkt);
        assert_eq!(items.len(), 1);
        match &items[0] {
            RtcpItem::SenderReport {
                ssrc,
                ntp_timestamp,
                rtp_timestamp,
                packet_count,
                octet_count,
                blocks,
            } => {
                assert_eq!(*ssrc, 0xAAAA_BBBB);
                assert_eq!(*ntp_timestamp, 0x1122_3344_5566_7788);
                assert_eq!(*rtp_timestamp, 999);
                assert_eq!(*packet_count, 10);
                assert_eq!(*octet_count, 2000);
                assert_eq!(blocks, &[b]);
            }
            other => panic!("expected SenderReport, got {other:?}"),
        }
    }

    #[test]
    fn a_receiver_report_with_no_block_states_rc_zero() {
        let pkt = build_receiver_report(0x1111_2222, None);
        let items = parse_compound(&pkt);
        match &items[0] {
            RtcpItem::ReceiverReport { ssrc, blocks } => {
                assert_eq!(*ssrc, 0x1111_2222);
                assert!(blocks.is_empty());
            }
            other => panic!("expected ReceiverReport, got {other:?}"),
        }
    }

    #[test]
    fn a_compound_of_sr_and_sdes_parses_to_both_in_order() {
        let sr = build_sender_report(1, 0, 0, 0, 0, None);
        let sdes = build_source_description(1, "gsm-sip-bridge@line0");
        let pkt = compound(&[&sr, &sdes]);
        let items = parse_compound(&pkt);
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], RtcpItem::SenderReport { .. }));
        match &items[1] {
            RtcpItem::SourceDescription { ssrc, cname } => {
                assert_eq!(*ssrc, 1);
                assert_eq!(cname.as_deref(), Some("gsm-sip-bridge@line0"));
            }
            other => panic!("expected SourceDescription, got {other:?}"),
        }
    }

    #[test]
    fn a_bye_names_its_ssrc() {
        let pkt = build_bye(0xDEAD_BEEF);
        let items = parse_compound(&pkt);
        match &items[0] {
            RtcpItem::Bye { ssrcs } => assert_eq!(ssrcs, &[0xDEAD_BEEF]),
            other => panic!("expected Bye, got {other:?}"),
        }
    }

    // ---- parsing robustness -----------------------------------------

    #[test]
    fn empty_input_parses_to_nothing_without_panicking() {
        assert!(parse_compound(&[]).is_empty());
    }

    #[test]
    fn a_packet_truncated_mid_report_block_returns_only_what_parsed_before_it() {
        let sr = build_sender_report(1, 0, 0, 0, 0, None);
        let sdes = build_source_description(1, "x");
        let mut pkt = compound(&[&sr, &sdes]);
        pkt.truncate(sr.len() + 2); // chop the SDES packet mid-header
        let items = parse_compound(&pkt);
        assert_eq!(items.len(), 1, "only the intact SR should have parsed");
        assert!(matches!(items[0], RtcpItem::SenderReport { .. }));
    }

    #[test]
    fn an_unrecognised_payload_type_does_not_abort_later_members() {
        // APP (204) is out of scope (module doc) but must not poison the rest
        // of the compound packet.
        let mut app = vec![RTCP_VERSION << 6, 204, 0, 1];
        app.extend_from_slice(&[0u8; 4]); // one 32-bit word of body
        let sr = build_sender_report(1, 0, 0, 0, 0, None);
        let pkt = compound(&[&app, &sr]);
        let items = parse_compound(&pkt);
        assert_eq!(items.len(), 2);
        assert!(matches!(items[0], RtcpItem::Unknown { pt: 204 }));
        assert!(matches!(items[1], RtcpItem::SenderReport { .. }));
    }

    // ---- round trip ----------------------------------------------------

    #[test]
    fn round_trip_derives_from_a_known_lsr_dlsr_now_triple() {
        let lsr = 0x0001_0000; // 1.0s in Q16.16 mid-32 units
        let dlsr = 0x0000_8000; // 0.5s
        let expected_rtt = 0x0000_4000u32; // 0.25s
        let now = lsr + dlsr + expected_rtt;
        let rtt = derive_round_trip(now, lsr, dlsr).unwrap();
        assert!((rtt.as_secs_f64() - 0.25).abs() < 1e-6, "got {rtt:?}");
    }

    #[test]
    fn round_trip_is_none_when_no_sr_has_ever_been_acknowledged() {
        assert_eq!(derive_round_trip(12345, 0, 0), None);
    }

    #[test]
    fn round_trip_is_none_rather_than_a_huge_duration_on_clock_skew() {
        // now - lsr - dlsr would go negative; wrapping makes it huge instead.
        assert_eq!(derive_round_trip(0, 0x0001_0000, 0), None);
    }

    // ---- schedule --------------------------------------------------

    #[test]
    fn is_due_is_false_immediately_after_construction() {
        let t0 = Instant::now();
        let mut sched = ReportSchedule::new(800, t0);
        assert!(!sched.is_due(t0));
    }

    #[test]
    fn is_due_becomes_true_once_the_deadline_passes() {
        let t0 = Instant::now();
        let mut sched = ReportSchedule::new(800, t0);
        let due = sched.next_due();
        assert!(sched.is_due(due));
    }

    #[test]
    fn randomised_intervals_stay_within_the_rfc3550_band_and_average_near_base() {
        let t0 = Instant::now();
        let mut sched = ReportSchedule::new(800, t0);
        sched.record_packet_size(100); // fix the mean so the base is stable
        let base = sched.base_interval();
        let mut now = sched.next_due();
        let mut deltas = Vec::new();
        for _ in 0..200 {
            sched.record_packet_size(100);
            assert!(sched.is_due(now));
            let new_due = sched.next_due();
            deltas.push(new_due.duration_since(now).as_secs_f64());
            now = new_due;
        }
        let lower = base.as_secs_f64() * 0.5;
        let upper = base.as_secs_f64() * 1.5;
        for d in &deltas {
            assert!(
                *d >= lower - 1e-6 && *d <= upper + 1e-6,
                "{d} outside [{lower},{upper}]"
            );
        }
        let mean: f64 = deltas.iter().sum::<f64>() / deltas.len() as f64;
        assert!(
            (mean - base.as_secs_f64()).abs() < base.as_secs_f64() * 0.15,
            "mean {mean} too far from base {}",
            base.as_secs_f64()
        );
    }

    // ---- source guard ------------------------------------------------

    fn addr(ip: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]),
            port,
        ))
    }

    #[test]
    fn a_datagram_from_the_peer_ip_on_a_different_port_is_accepted() {
        let mut guard = SourceGuard::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(guard.accept(addr([10, 0, 0, 1], 55555)));
    }

    #[test]
    fn a_datagram_from_a_different_ip_is_rejected() {
        let mut guard = SourceGuard::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(!guard.accept(addr([10, 0, 0, 2], 4000)));
        assert_eq!(guard.rejected_count(), 1);
    }

    #[test]
    fn rapid_rejections_are_rate_limited_in_logging_but_still_all_counted() {
        let mut guard = SourceGuard::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)));
        for _ in 0..10 {
            assert!(!guard.accept(addr([10, 0, 0, 9], 4000)));
        }
        assert_eq!(guard.rejected_count(), 10, "every rejection is counted");
    }

    // ---- endpoint bind -------------------------------------------------

    #[test]
    fn binding_never_fails_and_always_lands_tier_one_or_two_on_loopback() {
        for _ in 0..20 {
            let bound = bind_rtp_and_rtcp(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
            match bound.rtcp {
                Some((_, port, declared)) => {
                    if !declared {
                        assert_eq!(bound.rtp_port % 2, 0, "tier 1 requires an even RTP port");
                        assert_eq!(port, bound.rtp_port + 1, "tier 1's RTCP port is RTP+1");
                    }
                }
                None => panic!("tier 3 should not occur on a healthy loopback"),
            }
        }
    }

    // ---- far-end quality -----------------------------------------------

    #[test]
    fn the_first_report_moves_reports_received_from_zero_to_one() {
        let mut q = FarEndQuality::default();
        assert_eq!(q.reports_received, 0);
        q.record(&block(1), 8000, 0);
        assert_eq!(q.reports_received, 1);
        assert_eq!(q.fraction_lost, Some(5));
        assert_eq!(q.cumulative_lost, Some(3));
        assert!(q.jitter.is_some());
    }

    // ---- build_report / handle_inbound_item ---------------------------

    #[test]
    fn build_report_returns_empty_when_there_is_nothing_to_report() {
        let bundle = CarrierRtcpBundle::new();
        assert!(build_report(&bundle, 8000, "cname").is_empty());
    }

    #[test]
    fn build_report_sends_a_sender_report_with_a_cname_once_something_was_sent() {
        let bundle = CarrierRtcpBundle::new();
        bundle.send.record_sent(0xAAAA, 160, 320);
        let pkt = build_report(&bundle, 8000, "gsm-sip-bridge@line0");
        let items = parse_compound(&pkt);
        assert!(
            items.iter().any(|i| matches!(
                i,
                RtcpItem::SenderReport {
                    ssrc: 0xAAAA,
                    packet_count: 1,
                    ..
                }
            )),
            "{items:?}"
        );
        assert!(
            items.iter().any(|i| matches!(
                i,
                RtcpItem::SourceDescription { ssrc: 0xAAAA, cname: Some(c) }
                    if c == "gsm-sip-bridge@line0"
            )),
            "{items:?}"
        );
    }

    #[test]
    fn build_report_sends_a_receiver_report_when_only_receiving_ever_happened() {
        let bundle = CarrierRtcpBundle::new();
        *bundle.receive_ssrc.lock().unwrap() = Some(0xBEEF);
        bundle
            .receive
            .lock()
            .unwrap()
            .on_packet(1, 0, Duration::ZERO, 8000);
        let pkt = build_report(&bundle, 8000, "cname");
        let items = parse_compound(&pkt);
        assert!(
            items
                .iter()
                .any(|i| matches!(i, RtcpItem::ReceiverReport { ssrc: 0xBEEF, .. })),
            "{items:?}"
        );
    }

    #[test]
    fn handle_inbound_item_routes_a_matching_block_from_either_sr_or_rr() {
        let bundle = CarrierRtcpBundle::new();
        bundle.send.record_sent(0x1234, 160, 0);
        let b = block(0x1234);

        handle_inbound_item(
            &RtcpItem::ReceiverReport {
                ssrc: 999,
                blocks: vec![b],
            },
            &bundle,
            8000,
        );
        assert_eq!(bundle.far_end.lock().unwrap().reports_received, 1);

        handle_inbound_item(
            &RtcpItem::SenderReport {
                ssrc: 999,
                ntp_timestamp: 0,
                rtp_timestamp: 0,
                packet_count: 0,
                octet_count: 0,
                blocks: vec![b],
            },
            &bundle,
            8000,
        );
        assert_eq!(
            bundle.far_end.lock().unwrap().reports_received,
            2,
            "a report block about us, wrapped in either SR or RR, must be read"
        );
    }

    #[test]
    fn handle_inbound_item_ignores_a_block_about_a_different_source() {
        let bundle = CarrierRtcpBundle::new();
        bundle.send.record_sent(0x1234, 160, 0);
        handle_inbound_item(
            &RtcpItem::ReceiverReport {
                ssrc: 999,
                blocks: vec![block(0x9999)],
            },
            &bundle,
            8000,
        );
        assert_eq!(bundle.far_end.lock().unwrap().reports_received, 0);
    }

    #[test]
    fn handle_inbound_item_does_nothing_before_anything_has_been_sent() {
        // Nothing to correlate a report against yet — no our_ssrc exists.
        let bundle = CarrierRtcpBundle::new();
        handle_inbound_item(
            &RtcpItem::ReceiverReport {
                ssrc: 999,
                blocks: vec![block(0x1234)],
            },
            &bundle,
            8000,
        );
        assert_eq!(bundle.far_end.lock().unwrap().reports_received, 0);
    }
}
