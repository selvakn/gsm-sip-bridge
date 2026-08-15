//! Per-call RTP transmit/receive threads.
//!
//! Two independence properties, both load-bearing:
//!
//! 1. **No channel from the receive thread to the transmit thread.** The
//!    transmit stream is a pure function of an absolute frame counter, so
//!    total receive failure cannot change what is sent — otherwise `SendOnly`
//!    and `Neither` collapse into each other (`ims/echo.rs`'s warning,
//!    research.md R8). [`generate_frame`] takes no state but the frame index.
//! 2. **Absolute-deadline scheduling.** Each frame's send time is
//!    `start + n * ptime`, not `sleep(ptime)` after doing the work — the
//!    latter drifts by the per-packet work time and would corrupt a
//!    round-trip delay measurement (`ims/call.rs:609` has this bug; not
//!    repeated here).

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::media_stats::ReceiveTracker;
use gsm_sip_bridge::ims::rtp::{build_packet, parse_packet, WavWriter};

use crate::media::codec::{decode_pcmu, encode_pcmu, CodecProfile};
use crate::media::goertzel::SymbolDecoder;
use crate::media::level::LevelMeter;
use crate::media::tone;

const PTIME: Duration = Duration::from_millis(20);
/// Bounds the round-trip search window to a handful of symbol cycles
/// (16 symbols × 100ms ≈ 1.6s each) — comfortably more than any real
/// VoWiFi/VoLTE round trip, without growing unbounded over a long call.
const TX_TIMELINE_CAP: usize = 64;

/// The grid8 tone-plan signal (`media::tone`), or a placeholder silence when
/// the tone plan is disabled — a pure function of `frame_index` alone; it
/// must not read anything about what has been received.
pub fn generate_frame(frame_index: u64, codec: &CodecProfile, tone_enabled: bool) -> Vec<i16> {
    let n = codec.samples_per_frame;
    if !tone_enabled {
        return vec![0i16; n];
    }
    let sample_index = frame_index * n as u64;
    tone::generate(sample_index, n, codec.audio_hz)
}

pub struct MediaSessionConfig {
    pub local_rtp: SocketAddr,
    pub remote_rtp: SocketAddr,
    pub codec: CodecProfile,
    pub duration: Duration,
    pub sent_wav_path: Option<std::path::PathBuf>,
    pub received_wav_path: Option<std::path::PathBuf>,
    /// Whether to transmit the grid8 tone plan (US4) or silence — off by
    /// default keeps a call's audio content trivial for callers that only
    /// care about the packet-count verdict.
    pub tone_enabled: bool,
}

#[derive(Debug, Clone)]
pub struct LevelStats {
    pub peak_dbfs: f64,
    pub mean_dbfs: f64,
    pub noise_floor_dbfs: f64,
    pub silent_frame_pct: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ToneStats {
    pub tx_symbols_sent: u64,
    pub rx_symbols_detected: u64,
    pub expected_symbols: u64,
    pub first_detected_ms_after_start: Option<u64>,
    /// Round-trip samples in milliseconds — every time a symbol we sent was
    /// heard coming back. Empty when the signal never looped back (no
    /// acoustic/network loopback path), which is reported as unmeasured, not
    /// as a failure.
    pub rtt_samples_ms: Vec<u64>,
}

pub struct MediaSessionResult {
    pub sent_packets: u64,
    pub receive_stats: gsm_sip_bridge::ims::media_stats::ReceiveStats,
    pub level: LevelStats,
    pub tone: ToneStats,
}

/// Binds the RTP socket, runs tx/rx threads for `config.duration` (or until
/// `stop` is set externally), and returns the counters needed to build a
/// [`crate::media::report::CallReport`].
pub fn run(
    config: MediaSessionConfig,
    stop: Arc<AtomicBool>,
) -> std::io::Result<MediaSessionResult> {
    let socket = UdpSocket::bind(config.local_rtp)?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    socket.connect(config.remote_rtp)?;
    let socket = Arc::new(socket);

    let sent_packets = Arc::new(AtomicU64::new(0));
    let tracker = Arc::new(Mutex::new(ReceiveTracker::new()));
    let tx_timeline: Arc<Mutex<VecDeque<(usize, Instant)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let ssrc: u32 = rand::random();

    let sent_wav = config
        .sent_wav_path
        .as_deref()
        .and_then(|p| WavWriter::create(p, config.codec.audio_hz).ok())
        .map(Mutex::new)
        .map(Arc::new);
    let received_wav = config
        .received_wav_path
        .as_deref()
        .and_then(|p| WavWriter::create(p, config.codec.audio_hz).ok())
        .map(Mutex::new)
        .map(Arc::new);

    let tx_handle = {
        let socket = socket.clone();
        let stop = stop.clone();
        let sent_packets = sent_packets.clone();
        let codec = config.codec;
        let sent_wav = sent_wav.clone();
        let tx_timeline = tx_timeline.clone();
        let tone_enabled = config.tone_enabled;
        thread::spawn(move || {
            tx_loop(
                socket,
                codec,
                ssrc,
                stop,
                sent_packets,
                sent_wav,
                tx_timeline,
                tone_enabled,
            )
        })
    };

    let rx_result: Arc<Mutex<(LevelStats, ToneStats)>> = Arc::new(Mutex::new((
        LevelStats {
            peak_dbfs: -120.0,
            mean_dbfs: -120.0,
            noise_floor_dbfs: -120.0,
            silent_frame_pct: 100.0,
        },
        ToneStats::default(),
    )));
    let rx_handle = {
        let socket = socket.clone();
        let stop = stop.clone();
        let tracker = tracker.clone();
        let codec = config.codec;
        let tx_timeline = tx_timeline.clone();
        let rx_result = rx_result.clone();
        let tone_enabled = config.tone_enabled;
        thread::spawn(move || {
            rx_loop(
                socket,
                codec,
                stop,
                tracker,
                received_wav,
                tx_timeline,
                rx_result,
                tone_enabled,
            )
        })
    };

    thread::sleep(config.duration);
    stop.store(true, Ordering::Relaxed);

    let _ = tx_handle.join();
    let _ = rx_handle.join();

    // `sent_wav` is finished here: `run()` kept the original `Arc`, tx_loop
    // only ever held a clone, and that clone was dropped when the thread
    // exited above, so the strong count is 1 by this point.
    if let Some(w) = sent_wav {
        if let Ok(w) = Arc::try_unwrap(w) {
            if let Ok(w) = w.into_inner() {
                let _ = w.finish();
            }
        }
    }
    // `received_wav` was moved into rx_loop by value (never cloned), so it
    // finishes itself at the end of that function instead.

    let receive_stats = tracker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .stats(config.codec.rtp_clock_hz);
    let (level, mut tone) = Arc::try_unwrap(rx_result)
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .unwrap_or_else(|_| {
            (
                LevelStats {
                    peak_dbfs: -120.0,
                    mean_dbfs: -120.0,
                    noise_floor_dbfs: -120.0,
                    silent_frame_pct: 100.0,
                },
                ToneStats::default(),
            )
        });
    tone.tx_symbols_sent = tx_timeline.lock().unwrap_or_else(|e| e.into_inner()).len() as u64;
    tone.expected_symbols = (config.duration.as_millis() as u64) / tone::SYMBOL_MS;

    Ok(MediaSessionResult {
        sent_packets: sent_packets.load(Ordering::Relaxed),
        receive_stats,
        level,
        tone,
    })
}

#[allow(clippy::too_many_arguments)]
fn tx_loop(
    socket: Arc<UdpSocket>,
    codec: CodecProfile,
    ssrc: u32,
    stop: Arc<AtomicBool>,
    sent_packets: Arc<AtomicU64>,
    sent_wav: Option<Arc<Mutex<WavWriter>>>,
    tx_timeline: Arc<Mutex<VecDeque<(usize, Instant)>>>,
    tone_enabled: bool,
) {
    let start = Instant::now();
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let mut n: u64 = 0;
    let mut last_symbol: Option<usize> = None;

    while !stop.load(Ordering::Relaxed) {
        let samples = generate_frame(n, &codec, tone_enabled);

        if tone_enabled {
            let sample_index = n * codec.samples_per_frame as u64;
            let symbol = tone::symbol_index_at(sample_index, codec.audio_hz);
            if last_symbol != Some(symbol) {
                last_symbol = Some(symbol);
                let mut tl = tx_timeline.lock().unwrap_or_else(|e| e.into_inner());
                if tl.len() >= TX_TIMELINE_CAP {
                    tl.pop_front();
                }
                tl.push_back((symbol, Instant::now()));
            }
        }

        if let Some(w) = &sent_wav {
            if let Ok(mut w) = w.lock() {
                let _ = w.write_samples(&samples);
            }
        }
        let payload = encode_pcmu(&samples);
        let pkt = build_packet(seq, ts, ssrc, codec.pt, &payload);
        let _ = socket.send(&pkt);
        sent_packets.fetch_add(1, Ordering::Relaxed);

        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(codec.ts_increment);
        n += 1;

        let deadline = start + PTIME * n as u32;
        let now = Instant::now();
        if deadline > now {
            thread::sleep(deadline - now);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rx_loop(
    socket: Arc<UdpSocket>,
    codec: CodecProfile,
    stop: Arc<AtomicBool>,
    tracker: Arc<Mutex<ReceiveTracker>>,
    received_wav: Option<Arc<Mutex<WavWriter>>>,
    tx_timeline: Arc<Mutex<VecDeque<(usize, Instant)>>>,
    rx_result: Arc<Mutex<(LevelStats, ToneStats)>>,
    tone_enabled: bool,
) {
    let start = Instant::now();
    let mut buf = [0u8; 2048];
    let mut level = LevelMeter::new();
    let mut decoder = SymbolDecoder::new(codec.audio_hz, PTIME.as_millis() as u64);
    let mut tone_stats = ToneStats::default();
    let mut rtt_samples: Vec<u64> = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        match socket.recv(&mut buf) {
            Ok(n) => {
                if let Some(parsed) = parse_packet(&buf[..n]) {
                    if parsed.payload_type == codec.pt {
                        let arrival = start.elapsed();
                        if let Ok(mut t) = tracker.lock() {
                            t.on_packet(parsed.seq, parsed.timestamp, arrival, codec.rtp_clock_hz);
                        }
                        let samples = decode_pcmu(parsed.payload);
                        if let Some(w) = &received_wav {
                            if let Ok(mut w) = w.lock() {
                                let _ = w.write_samples(&samples);
                            }
                        }
                        level.feed(&samples);

                        if tone_enabled {
                            if let Some(symbol) = decoder.feed(&samples) {
                                let now = Instant::now();
                                tone_stats.rx_symbols_detected += 1;
                                if tone_stats.first_detected_ms_after_start.is_none() {
                                    tone_stats.first_detected_ms_after_start =
                                        Some(start.elapsed().as_millis() as u64);
                                }
                                let tl = tx_timeline.lock().unwrap_or_else(|e| e.into_inner());
                                if let Some(&(_, tx_at)) =
                                    tl.iter().rev().find(|(s, t)| *s == symbol && *t <= now)
                                {
                                    let rtt = now.duration_since(tx_at);
                                    rtt_samples.push(rtt.as_millis() as u64);
                                }
                            }
                        }
                    }
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

    if let Some(w) = received_wav {
        if let Ok(w) = Arc::try_unwrap(w) {
            if let Ok(w) = w.into_inner() {
                let _ = w.finish();
            }
        }
    }

    tone_stats.rtt_samples_ms = rtt_samples;
    let level_stats = LevelStats {
        peak_dbfs: level.peak_dbfs(),
        mean_dbfs: level.mean_dbfs(),
        noise_floor_dbfs: level.noise_floor_dbfs(),
        silent_frame_pct: level.silent_frame_pct(),
    };
    *rx_result.lock().unwrap_or_else(|e| e.into_inner()) = (level_stats, tone_stats);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::codec::PCMU;

    /// The `ims::echo` independence invariant, structurally: the transmit
    /// stream is a pure function of the frame index, so it is byte-identical
    /// on every call regardless of anything a receiver might or might not
    /// have sent back. There is no state to attach a receiver's influence to
    /// in the first place.
    #[test]
    fn transmit_stream_is_identical_regardless_of_what_was_received() {
        let a: Vec<Vec<i16>> = (0..50).map(|n| generate_frame(n, &PCMU, true)).collect();
        let b: Vec<Vec<i16>> = (0..50).map(|n| generate_frame(n, &PCMU, true)).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn media_session_round_trips_over_loopback() {
        use std::net::UdpSocket as StdUdp;

        // A trivial UAS-side loopback: echo every PCMU packet straight back.
        let far_socket = StdUdp::bind("127.0.0.1:0").unwrap();
        far_socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let far_addr = far_socket.local_addr().unwrap();
        let echo_stop = Arc::new(AtomicBool::new(false));
        let echo_handle = {
            let stop = echo_stop.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, src)) = far_socket.recv_from(&mut buf) {
                        let _ = far_socket.send_to(&buf[..n], src);
                    }
                }
            })
        };

        let config = MediaSessionConfig {
            local_rtp: "127.0.0.1:0".parse().unwrap(),
            remote_rtp: far_addr,
            codec: PCMU,
            duration: Duration::from_millis(500),
            sent_wav_path: None,
            received_wav_path: None,
            tone_enabled: false,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let result = run(config, stop).unwrap();

        echo_stop.store(true, Ordering::Relaxed);
        let _ = echo_handle.join();

        assert!(
            result.sent_packets > 10,
            "expected several packets sent in 500ms, got {}",
            result.sent_packets
        );
        assert!(
            result.receive_stats.received_packets > 0,
            "expected the loopback echo to produce received packets"
        );
    }

    /// The tone-loopback path this feature exists to prove: with the tone
    /// plan on and a real echo, both directions carry our signal and a
    /// plausible non-zero RTT comes back — filling
    /// `MediaReport::round_trip_delay`'s long-standing `None`
    /// (`gsm-sip-bridge/src/ims/call.rs:153`).
    #[test]
    fn tone_plan_is_detected_on_both_sides_of_a_real_loopback_with_measurable_rtt() {
        use std::net::UdpSocket as StdUdp;

        let far_socket = StdUdp::bind("127.0.0.1:0").unwrap();
        far_socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let far_addr = far_socket.local_addr().unwrap();
        let echo_stop = Arc::new(AtomicBool::new(false));
        let echo_handle = {
            let stop = echo_stop.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 2048];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, src)) = far_socket.recv_from(&mut buf) {
                        // A deliberate fixed delay gives the RTT assertion below
                        // ground truth to check against.
                        thread::sleep(Duration::from_millis(20));
                        let _ = far_socket.send_to(&buf[..n], src);
                    }
                }
            })
        };

        let config = MediaSessionConfig {
            local_rtp: "127.0.0.1:0".parse().unwrap(),
            remote_rtp: far_addr,
            codec: PCMU,
            duration: Duration::from_millis(2500),
            sent_wav_path: None,
            received_wav_path: None,
            tone_enabled: true,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let result = run(config, stop).unwrap();

        echo_stop.store(true, Ordering::Relaxed);
        let _ = echo_handle.join();

        assert!(
            result.tone.rx_symbols_detected > 0,
            "expected the looped-back tone to be detected"
        );
        assert!(
            !result.tone.rtt_samples_ms.is_empty(),
            "expected at least one round-trip measurement"
        );
        let avg_rtt: u64 = result.tone.rtt_samples_ms.iter().sum::<u64>()
            / result.tone.rtt_samples_ms.len() as u64;
        assert!(
            avg_rtt < 500,
            "RTT should be small on a local loopback with a 20ms artificial delay, got {avg_rtt}ms"
        );
    }
}
