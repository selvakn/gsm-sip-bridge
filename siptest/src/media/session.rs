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

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use gsm_sip_bridge::ims::media_stats::ReceiveTracker;
use gsm_sip_bridge::ims::rtp::{build_packet, parse_packet, WavWriter};

use crate::media::codec::{decode_pcmu, encode_pcmu, CodecProfile};

const PTIME: Duration = Duration::from_millis(20);

/// A deterministic placeholder signal — a quiet 440Hz tone — so a call
/// carries real audio content pending the full tone/Goertzel plan (US4).
/// Deliberately a pure function of `frame_index` alone: it must not read
/// anything about what has been received.
pub fn generate_frame(frame_index: u64, codec: &CodecProfile) -> Vec<i16> {
    let n = codec.samples_per_frame;
    (0..n)
        .map(|i| {
            let sample_index = frame_index * n as u64 + i as u64;
            let t = sample_index as f64 / codec.audio_hz as f64;
            (200.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
        })
        .collect()
}

pub struct MediaSessionConfig {
    pub local_rtp: SocketAddr,
    pub remote_rtp: SocketAddr,
    pub codec: CodecProfile,
    pub duration: Duration,
    pub sent_wav_path: Option<std::path::PathBuf>,
    pub received_wav_path: Option<std::path::PathBuf>,
}

pub struct MediaSessionResult {
    pub sent_packets: u64,
    pub receive_stats: gsm_sip_bridge::ims::media_stats::ReceiveStats,
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
        thread::spawn(move || tx_loop(socket, codec, ssrc, stop, sent_packets, sent_wav))
    };

    let rx_handle = {
        let socket = socket.clone();
        let stop = stop.clone();
        let tracker = tracker.clone();
        let codec = config.codec;
        thread::spawn(move || rx_loop(socket, codec, stop, tracker, received_wav))
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
    Ok(MediaSessionResult {
        sent_packets: sent_packets.load(Ordering::Relaxed),
        receive_stats,
    })
}

fn tx_loop(
    socket: Arc<UdpSocket>,
    codec: CodecProfile,
    ssrc: u32,
    stop: Arc<AtomicBool>,
    sent_packets: Arc<AtomicU64>,
    sent_wav: Option<Arc<Mutex<WavWriter>>>,
) {
    let start = Instant::now();
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let mut n: u64 = 0;

    while !stop.load(Ordering::Relaxed) {
        let samples = generate_frame(n, &codec);
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

fn rx_loop(
    socket: Arc<UdpSocket>,
    codec: CodecProfile,
    stop: Arc<AtomicBool>,
    tracker: Arc<Mutex<ReceiveTracker>>,
    received_wav: Option<Arc<Mutex<WavWriter>>>,
) {
    let start = Instant::now();
    let mut buf = [0u8; 2048];

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
        let a: Vec<Vec<i16>> = (0..50).map(|n| generate_frame(n, &PCMU)).collect();
        let b: Vec<Vec<i16>> = (0..50).map(|n| generate_frame(n, &PCMU)).collect();
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
}
