//! UDP-level regression tests: ordering must happen before the stateful AMR
//! decoder, and the original downlink RTP timeline must survive transcoding.
#![cfg(feature = "amr-linked")]

use super::*;
use std::time::Instant;

const SOURCE_SSRC: u32 = 0x1234_abcd;
const REORDER_WAIT: Duration = Duration::from_millis(40);

fn codec(codec: NegotiatedCodec, payload_type: u8) -> ChosenCodec {
    ChosenCodec {
        codec,
        payload_type,
        octet_aligned: true,
        dtmf_payload_type: None,
    }
}

fn connected_pair() -> (UdpSocket, UdpSocket) {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    a.connect(b.local_addr().unwrap()).unwrap();
    b.connect(a.local_addr().unwrap()).unwrap();
    (a, b)
}

struct Relay {
    carrier: UdpSocket,
    pbx: UdpSocket,
    stop: Arc<AtomicBool>,
}

impl Relay {
    fn start(carrier_codec: ChosenCodec, pbx_codec: ChosenCodec, wait: Duration) -> Self {
        let (ims, carrier) = connected_pair();
        let (veth, pbx) = connected_pair();
        for socket in [&carrier, &pbx] {
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        spawn_transcoding_relay(
            ims,
            veth,
            carrier_codec,
            pbx_codec,
            stop.clone(),
            &super::super::media_stats::MediaMeter::new(),
            None,
            wait,
        )
        .unwrap();
        Self { carrier, pbx, stop }
    }

    fn downlink(&self, packet: &[u8]) {
        self.carrier.send(packet).unwrap();
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Received {
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload_type: u8,
    marker: bool,
    payload: Vec<u8>,
}

fn receive(socket: &UdpSocket) -> Received {
    let mut buf = [0u8; RECV_BUF];
    let n = socket
        .recv(&mut buf)
        .expect("relay must emit an RTP packet");
    let packet = rtp::parse_packet(&buf[..n]).unwrap();
    Received {
        seq: packet.seq,
        timestamp: packet.timestamp,
        ssrc: packet.ssrc,
        payload_type: packet.payload_type,
        marker: buf[1] & 0x80 != 0,
        payload: packet.payload.to_vec(),
    }
}

fn assert_no_packet(socket: &UdpSocket, timeout: Duration) {
    socket.set_read_timeout(Some(timeout)).unwrap();
    let error = socket.recv(&mut [0u8; RECV_BUF]).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ));
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
}

fn source_packet(seq: u16, timestamp: u32, payload_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = rtp::build_packet(seq, timestamp, SOURCE_SSRC, payload_type, payload);
    if seq.is_multiple_of(2) {
        packet[1] |= 0x80;
    }
    packet
}

/// Continuous speech-like input gives every AMR frame its own content and
/// history; decoding frames in arrival order cannot accidentally pass by
/// merely sorting their RTP headers afterwards.
fn speech_payloads(kind: AmrKind, octet_aligned: bool, count: usize) -> Vec<Vec<u8>> {
    let mut encoder = match kind {
        AmrKind::Nb => AmrEncoder::Nb(amr_safe::NbEncoder::new().unwrap()),
        AmrKind::Wb => AmrEncoder::Wb(amr_safe::WbEncoder::new().unwrap()),
    };
    let samples = match kind {
        AmrKind::Nb => amr_safe::NB_FRAME_SAMPLES,
        AmrKind::Wb => amr_safe::FRAME_SAMPLES,
    };
    (0..count)
        .map(|frame_index| {
            let pcm: Vec<i16> = (0..samples)
                .map(|sample_index| {
                    let position = (frame_index * samples + sample_index) as f64;
                    let frequency = 0.08 + frame_index as f64 * 0.013;
                    (position.mul_add(frequency, 0.0).sin() * 12_000.0) as i16
                })
                .collect();
            let frame = encoder.encode(&pcm).unwrap();
            amr_rtp::frame_to_payload(&frame, kind, octet_aligned).unwrap()
        })
        .collect()
}

fn check_shuffled_amr(kind: AmrKind, octet_aligned: bool) {
    let (carrier_codec, pbx_codec) = match kind {
        AmrKind::Nb => (NegotiatedCodec::AmrNb, NegotiatedCodec::Pcmu),
        AmrKind::Wb => (NegotiatedCodec::AmrWb, NegotiatedCodec::L16),
    };
    let mut carrier = codec(carrier_codec, 97);
    carrier.octet_aligned = octet_aligned;
    let pbx = codec(pbx_codec, 96);
    let timestamp_step = carrier_codec.frame_samples() as u32;
    let packets: Vec<_> = speech_payloads(kind, octet_aligned, 5)
        .iter()
        .enumerate()
        .map(|(i, payload)| {
            source_packet(
                700 + i as u16,
                8_500 + i as u32 * timestamp_step,
                97,
                payload,
            )
        })
        .collect();

    let ordered = Relay::start(carrier, pbx, REORDER_WAIT);
    let expected: Vec<_> = packets
        .iter()
        .map(|packet| {
            ordered.downlink(packet);
            receive(&ordered.pbx)
        })
        .collect();
    let shuffled = Relay::start(carrier, pbx, REORDER_WAIT);
    shuffled.downlink(&packets[0]);
    assert_eq!(receive(&shuffled.pbx), expected[0]);
    for index in [3, 2, 1, 4] {
        shuffled.downlink(&packets[index]);
    }
    for (index, expected) in expected.iter().enumerate().skip(1) {
        let actual = receive(&shuffled.pbx);
        assert_eq!(
            &actual, expected,
            "AMR frame {index} must decode in source order"
        );
        assert_eq!(actual.seq, 700 + index as u16);
        assert_eq!(actual.timestamp, 8_500 + index as u32 * timestamp_step);
        assert_eq!(actual.ssrc, SOURCE_SSRC);
        assert_eq!(actual.payload_type, 96);
        assert_eq!(actual.marker, actual.seq.is_multiple_of(2));
    }
}

#[test]
fn amr_nb_is_reordered_before_decoding_in_both_payload_formats() {
    check_shuffled_amr(AmrKind::Nb, true);
    check_shuffled_amr(AmrKind::Nb, false);
}

#[test]
fn amr_wb_is_reordered_before_decoding_in_both_payload_formats() {
    check_shuffled_amr(AmrKind::Wb, true);
    check_shuffled_amr(AmrKind::Wb, false);
}

#[test]
fn missing_packet_expires_without_further_input_and_late_packets_never_reach_decoder() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 6)
        .iter()
        .enumerate()
        .map(|(i, payload)| source_packet(10 + i as u16, 900 + i as u32 * 160, 97, payload))
        .collect();
    let baseline = Relay::start(carrier, pbx, Duration::ZERO);
    let expected: Vec<_> = [0, 2, 4, 5]
        .into_iter()
        .map(|index| {
            baseline.downlink(&packets[index]);
            receive(&baseline.pbx)
        })
        .collect();

    let relay = Relay::start(carrier, pbx, REORDER_WAIT);
    relay.downlink(&packets[0]);
    assert_eq!(receive(&relay.pbx), expected[0]);
    let sent_at = Instant::now();
    relay.downlink(&packets[2]);
    relay.downlink(&packets[4]);
    // The decoder cannot see frame 2 while frame 1 can still arrive.
    assert_no_packet(&relay.pbx, Duration::from_millis(10));
    let emitted = receive(&relay.pbx);
    assert!(sent_at.elapsed() >= Duration::from_millis(30));
    assert!(sent_at.elapsed() < Duration::from_millis(500));
    assert_eq!(emitted, expected[1]);
    assert_eq!((emitted.seq, emitted.timestamp), (12, 1_220));
    // A second hole in the accumulated batch does not start another wait.
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    assert_eq!(receive(&relay.pbx), expected[2]);
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Both a packet whose deadline passed and a duplicate of an already
    // emitted packet must be ignored before they can mutate decoder state.
    relay.downlink(&packets[1]);
    relay.downlink(&packets[2]);
    relay.downlink(&packets[3]);
    relay.downlink(&packets[5]);
    assert_eq!(receive(&relay.pbx), expected[3]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn zero_wait_skips_immediately_and_discards_late_or_duplicate_amr_before_decoding() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 4)
        .iter()
        .enumerate()
        .map(|(i, payload)| source_packet(10 + i as u16, 900 + i as u32 * 160, 97, payload))
        .collect();
    let baseline = Relay::start(carrier, pbx, Duration::ZERO);
    let expected: Vec<_> = [0, 2, 3]
        .into_iter()
        .map(|index| {
            baseline.downlink(&packets[index]);
            receive(&baseline.pbx)
        })
        .collect();
    let relay = Relay::start(carrier, pbx, Duration::ZERO);
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    relay.downlink(&packets[0]);
    assert_eq!(receive(&relay.pbx), expected[0]);
    relay.downlink(&packets[2]);
    assert_eq!(receive(&relay.pbx), expected[1]);
    relay.downlink(&packets[1]);
    relay.downlink(&packets[2]);
    relay.downlink(&packets[3]);
    assert_eq!(receive(&relay.pbx), expected[2]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn a_same_ssrc_sequence_restart_recovers_without_decoding_isolated_old_packets() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let payloads = speech_payloads(AmrKind::Nb, true, 6);
    let timestamps = [900_000, 900_160, 2_000, 2_160, 2_320, 2_480];

    // Keep codec input identical in the reference run, with a continuous
    // sequence run so it does not depend on restart detection itself.
    let baseline = Relay::start(carrier, pbx, REORDER_WAIT);
    let expected: Vec<_> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            baseline.downlink(&source_packet(
                10_000 + index as u16,
                timestamps[index],
                97,
                payload,
            ));
            receive(&baseline.pbx).payload
        })
        .collect();

    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    relay.downlink(&source_packet(10_000, timestamps[0], 97, &payloads[0]));
    assert_eq!(receive(&relay.pbx).payload, expected[0]);
    // One stale packet cannot reset the source or mutate the AMR decoder.
    // The following valid packet cancels this unconfirmed restart candidate.
    relay.downlink(&source_packet(99, 1_840, 97, &payloads[5]));
    assert_no_packet(&relay.pbx, Duration::from_millis(10));
    relay.downlink(&source_packet(10_001, timestamps[1], 97, &payloads[1]));
    let normal = receive(&relay.pbx);
    assert_eq!((normal.seq, normal.timestamp), (10_001, timestamps[1]));
    assert_eq!(normal.payload, expected[1]);

    // A restarted source may keep its SSRC while starting a much lower
    // sequence run. Consecutive packets must validate that new run promptly,
    // without waiting for sequence numbers to catch up or for the gap timer.
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    for (index, payload) in payloads.iter().enumerate().skip(2) {
        relay.downlink(&source_packet(
            100 + (index - 2) as u16,
            timestamps[index],
            97,
            payload,
        ));
    }
    for (index, expected_payload) in expected.iter().enumerate().skip(2) {
        let actual = receive(&relay.pbx);
        let seq = 100 + (index - 2) as u16;
        assert_eq!((actual.seq, actual.timestamp), (seq, timestamps[index]));
        assert_eq!(actual.ssrc, SOURCE_SSRC);
        assert_eq!(actual.marker, seq.is_multiple_of(2));
        assert_eq!(actual.payload_type, 0);
        assert_eq!(&actual.payload, expected_payload);
    }
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn consecutive_old_duplicates_cannot_restart_the_source_or_discard_buffered_amr() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 104)
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            source_packet(
                9_900 + index as u16,
                200_000 + index as u32 * 160,
                97,
                payload,
            )
        })
        .collect();

    let baseline = Relay::start(carrier, pbx, Duration::ZERO);
    let expected: Vec<_> = packets
        .iter()
        .map(|packet| {
            baseline.downlink(packet);
            receive(&baseline.pbx)
        })
        .collect();

    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    // Actually deliver the old frames before replaying them: their RTP
    // identities must remain recognizable beyond the recent-sequence guard.
    for (packet, expected) in packets.iter().zip(&expected).take(101) {
        relay.downlink(packet);
        assert_eq!(&receive(&relay.pbx), expected);
    }
    relay.downlink(&packets[103]); // Sequence 10003 waits for 10001.
    assert_no_packet(&relay.pbx, Duration::from_millis(10));

    // These are byte-identical copies of delivered sequences 9900 and 9901,
    // rather than new frames from a source that restarted its sequence run.
    relay.downlink(&packets[0]);
    relay.downlink(&packets[1]);
    assert_no_packet(&relay.pbx, Duration::from_millis(30));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    // Fill the original gap last so all three final frames leave together.
    // Both PCM and source headers must match a run without the duplicate pair.
    relay.downlink(&packets[102]);
    relay.downlink(&packets[101]);
    for expected in expected.iter().skip(101) {
        assert_eq!(&receive(&relay.pbx), expected);
    }
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn a_retired_ssrc_cannot_replace_the_active_source_or_clear_its_buffered_amr() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let payloads = speech_payloads(AmrKind::Nb, true, 5);
    let active_ssrc = 0x5566_eeff;
    let packet = |seq, timestamp, ssrc: u32, payload: &[u8]| {
        let mut packet = source_packet(seq, timestamp, 97, payload);
        packet[8..12].copy_from_slice(&ssrc.to_be_bytes());
        packet
    };
    let source_a = packet(10_000, 900_000, SOURCE_SSRC, &payloads[0]);
    let source_b: Vec<_> = payloads
        .iter()
        .enumerate()
        .skip(1)
        .take(3)
        .map(|(index, payload)| {
            packet(
                199 + index as u16,
                1_840 + index as u32 * 160,
                active_ssrc,
                payload,
            )
        })
        .collect();

    // The reference includes the same real source change and codec frames,
    // but no delayed packet from the source that has already been replaced.
    let baseline = Relay::start(carrier, pbx, REORDER_WAIT);
    baseline.downlink(&source_a);
    let expected_a = receive(&baseline.pbx);
    let expected_b: Vec<_> = source_b
        .iter()
        .map(|packet| {
            baseline.downlink(packet);
            receive(&baseline.pbx)
        })
        .collect();

    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    relay.downlink(&source_a);
    assert_eq!(receive(&relay.pbx), expected_a);
    relay.downlink(&source_b[0]);
    assert_eq!(receive(&relay.pbx), expected_b[0]);
    relay.downlink(&source_b[2]);
    assert_no_packet(&relay.pbx, Duration::from_millis(10));

    // A late packet from retired A must neither enter the stateful decoder
    // nor discard B's queued sequence 202 while sequence 201 is missing.
    relay.downlink(&packet(10_001, 900_160, SOURCE_SSRC, &payloads[4]));
    assert_no_packet(&relay.pbx, Duration::from_millis(30));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    relay.downlink(&source_b[1]);
    assert_eq!(receive(&relay.pbx), expected_b[1]);
    assert_eq!(receive(&relay.pbx), expected_b[2]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn an_interleaved_dtmf_packet_completes_the_shared_sequence_space() {
    let mut carrier = codec(NegotiatedCodec::AmrNb, 97);
    carrier.dtmf_payload_type = Some(100);
    let mut pbx = codec(NegotiatedCodec::Pcmu, 0);
    pbx.dtmf_payload_type = Some(101);
    // Long enough that a receive timeout can distinguish releasing on DTMF
    // arrival from incorrectly waiting for an absent audio sequence 101.
    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    let payloads = speech_payloads(AmrKind::Nb, true, 2);
    relay.downlink(&source_packet(100, 1_000, 97, &payloads[0]));
    receive(&relay.pbx);
    relay.downlink(&source_packet(102, 1_320, 97, &payloads[1]));
    assert_no_packet(&relay.pbx, Duration::from_millis(10));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let event = [1, 10, 0, 160];
    relay.downlink(&source_packet(101, 1_000, 100, &event));
    let first = receive(&relay.pbx);
    let second = receive(&relay.pbx);
    let audio = if first.payload_type == 0 {
        assert_eq!(second.payload_type, 101);
        assert_eq!(second.payload, event);
        first
    } else {
        assert_eq!(first.payload_type, 101);
        assert_eq!(first.payload, event);
        second
    };
    assert_eq!((audio.seq, audio.timestamp), (102, 1_320));
}

#[test]
fn a_separate_dtmf_ssrc_preserves_active_amr_and_its_pending_gap() {
    let mut carrier = codec(NegotiatedCodec::AmrNb, 97);
    carrier.dtmf_payload_type = Some(100);
    let mut pbx = codec(NegotiatedCodec::Pcmu, 0);
    pbx.dtmf_payload_type = Some(101);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 4)
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            source_packet(100 + index as u16, 1_000 + index as u32 * 160, 97, payload)
        })
        .collect();

    let baseline = Relay::start(carrier, pbx, REORDER_WAIT);
    let expected: Vec<_> = packets
        .iter()
        .map(|packet| {
            baseline.downlink(packet);
            receive(&baseline.pbx)
        })
        .collect();

    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    relay.downlink(&packets[0]);
    assert_eq!(receive(&relay.pbx), expected[0]);
    relay.downlink(&packets[2]);
    assert_no_packet(&relay.pbx, Duration::from_millis(10));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    // This event has an independent RTP source and sequence space. It must
    // be forwarded without replacing the active AMR source or its queued 102.
    let digit = [1, 10, 0, 160];
    relay.downlink(&rtp::build_packet(7_000, 5_000, 0x8765_4321, 100, &digit));
    let event = receive(&relay.pbx);
    assert_eq!(event.payload_type, 101);
    assert_eq!(event.payload, digit);
    assert_no_packet(&relay.pbx, Duration::from_millis(30));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();

    relay.downlink(&packets[1]);
    assert_eq!(receive(&relay.pbx), expected[1]);
    assert_eq!(receive(&relay.pbx), expected[2]);
    relay.downlink(&packets[3]);
    assert_eq!(receive(&relay.pbx), expected[3]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn separate_dtmf_before_the_first_audio_packet_does_not_establish_an_amr_source() {
    let mut carrier = codec(NegotiatedCodec::AmrNb, 97);
    carrier.dtmf_payload_type = Some(100);
    let mut pbx = codec(NegotiatedCodec::Pcmu, 0);
    pbx.dtmf_payload_type = Some(101);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 2)
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            source_packet(100 + index as u16, 4_000 + index as u32 * 160, 97, payload)
        })
        .collect();

    let baseline = Relay::start(carrier, pbx, REORDER_WAIT);
    let expected: Vec<_> = packets
        .iter()
        .map(|packet| {
            baseline.downlink(packet);
            receive(&baseline.pbx)
        })
        .collect();

    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let digit = [2, 10, 0, 160];
    relay.downlink(&rtp::build_packet(7_000, 5_000, 0x8765_4321, 100, &digit));
    let first_event = receive(&relay.pbx);
    assert_eq!(first_event.payload_type, 101);
    assert_eq!(first_event.payload, digit);
    relay.downlink(&packets[0]);
    assert_eq!(receive(&relay.pbx), expected[0]);

    // Starting AMR must not retire the previously seen independent DTMF
    // source, and its next event packet must not retire AMR in turn.
    let digit_repeat = [2, 10, 0, 240];
    relay.downlink(&rtp::build_packet(
        7_001,
        5_000,
        0x8765_4321,
        100,
        &digit_repeat,
    ));
    let repeat = receive(&relay.pbx);
    assert_eq!(repeat.payload_type, 101);
    assert_eq!(repeat.payload, digit_repeat);
    assert_eq!(repeat.timestamp, first_event.timestamp);
    assert_eq!(repeat.seq, first_event.seq.wrapping_add(1));
    relay.downlink(&packets[1]);
    assert_eq!(receive(&relay.pbx), expected[1]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn no_data_and_timestamp_wrap_preserve_the_original_timeline() {
    let relay = Relay::start(
        codec(NegotiatedCodec::AmrNb, 97),
        codec(NegotiatedCodec::Pcmu, 0),
        REORDER_WAIT,
    );
    let payloads = speech_payloads(AmrKind::Nb, true, 2);
    let initial_timestamp = u32::MAX - 79;
    relay.downlink(&source_packet(65_534, initial_timestamp, 97, &payloads[0]));
    let first = receive(&relay.pbx);
    assert_eq!(first.timestamp, initial_timestamp);
    // Octet-aligned CMR=15, FT=15 (NO_DATA), Q=1.
    relay.downlink(&source_packet(
        65_535,
        initial_timestamp.wrapping_add(160),
        97,
        &[0xf0, 0x7c],
    ));
    relay.downlink(&source_packet(
        0,
        initial_timestamp.wrapping_add(320),
        97,
        &payloads[1],
    ));
    let next = receive(&relay.pbx);
    assert_eq!(next.seq, 0);
    assert_eq!(next.timestamp, initial_timestamp.wrapping_add(320));
    assert_eq!(next.ssrc, SOURCE_SSRC);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn wideband_to_pcmu_scales_the_source_clock_continuously_through_wrap_and_no_data() {
    let relay = Relay::start(
        codec(NegotiatedCodec::AmrWb, 97),
        codec(NegotiatedCodec::Pcmu, 0),
        REORDER_WAIT,
    );
    let payloads = speech_payloads(AmrKind::Wb, true, 3);
    let source_start = u32::MAX - 159;
    let output_start = source_start / 2;
    relay.downlink(&source_packet(200, source_start, 97, &payloads[0]));
    let first = receive(&relay.pbx);
    assert_eq!((first.seq, first.timestamp), (200, output_start));
    assert_eq!(first.payload.len(), 160);

    // Both source timestamps wrap, but the destination clock must continue
    // from its own scaled starting point. Dividing each raw u32 timestamp
    // separately would insert a half-range discontinuity here.
    relay.downlink(&source_packet(
        202,
        source_start.wrapping_add(640),
        97,
        &payloads[1],
    ));
    relay.downlink(&source_packet(
        201,
        source_start.wrapping_add(320),
        97,
        &[0xf0, 0x7c],
    ));
    let second = receive(&relay.pbx);
    assert_eq!((second.seq, second.timestamp), (202, output_start + 320));
    assert_eq!(second.ssrc, SOURCE_SSRC);
    assert_eq!(second.payload.len(), 160);

    relay.downlink(&source_packet(
        203,
        source_start.wrapping_add(960),
        97,
        &payloads[2],
    ));
    let third = receive(&relay.pbx);
    assert_eq!((third.seq, third.timestamp), (203, output_start + 480));
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn uplink_keeps_its_generated_timeline_and_does_not_wait_for_source_sequence_gaps() {
    let relay = Relay::start(
        codec(NegotiatedCodec::AmrNb, 97),
        codec(NegotiatedCodec::Pcmu, 0),
        Duration::from_millis(500),
    );
    relay
        .carrier
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut received = Vec::new();
    for (seq, timestamp) in [(90, 200), (92, 9_000), (91, 100)] {
        relay
            .pbx
            .send(&source_packet(seq, timestamp, 0, &[0x55; 160]))
            .unwrap();
        received.push(receive(&relay.carrier));
    }
    for pair in received.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1));
        assert_eq!(pair[1].timestamp, pair[0].timestamp.wrapping_add(160));
        assert_eq!(pair[1].ssrc, pair[0].ssrc);
    }
    assert!(received.iter().all(|packet| packet.payload_type == 97));
    assert!(received
        .iter()
        .all(|packet| { amr_rtp::payload_to_frame(&packet.payload, AmrKind::Nb, true).is_some() }));
}
