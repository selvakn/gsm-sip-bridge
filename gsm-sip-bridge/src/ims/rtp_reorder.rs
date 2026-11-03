//! A bounded wait for a missing RTP sequence before carrier-side decoding.
//!
//! Packets retain their complete original headers and payloads. This buffer
//! only changes delivery order; it has no playout clock or codec knowledge.

use super::rtp;
use std::collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Recent late packets must not masquerade as a source restart, even when
/// several arrive in order. At 20ms ptime this covers two seconds of history.
const RECENT_SEQUENCE_HISTORY: u16 = 100;
/// Two far-back packets must belong to one short, consecutive run. An old
/// isolated packet cannot remain evidence for a restart indefinitely.
const RESTART_CONFIRMATION_WINDOW: Duration = Duration::from_secs(1);
/// Remember one full sequence cycle of deliveries, including distinct media
/// that reused a sequence after a restart. Bound memory even on long calls.
const DELIVERED_PACKET_HISTORY: usize = u16::MAX as usize + 1;

struct RestartCandidate {
    seq: u16,
    packet: Vec<u8>,
    since: Instant,
}

pub(super) struct RtpReorderBuffer {
    max_wait: Duration,
    ssrc: Option<u32>,
    // Remember every superseded source for this call: expiring or evicting
    // one would let a late packet reset the active source's decoder input.
    retired_ssrcs: HashSet<u32>,
    // Count occurrences so delivering a reused identity after sequence wrap
    // refreshes its history even when an older occurrence is then evicted.
    delivered: HashMap<(u16, u64), usize>,
    delivery_order: VecDeque<(u16, u64)>,
    expected: u16,
    pending: BTreeMap<u16, Vec<u8>>,
    deadline: Option<Instant>,
    restart_candidate: Option<RestartCandidate>,
}

impl RtpReorderBuffer {
    pub(super) fn new(max_wait: Duration) -> Self {
        Self {
            max_wait,
            ssrc: None,
            retired_ssrcs: HashSet::new(),
            delivered: HashMap::new(),
            delivery_order: VecDeque::new(),
            expected: 0,
            pending: BTreeMap::new(),
            deadline: None,
            restart_candidate: None,
        }
    }

    /// Delivers the first packet immediately. Later packets wait only when
    /// their sequence is ahead of the next expected one. Receiving that
    /// expected packet releases the entire batch in order, skipping any
    /// other holes. An expired wait does the same without the missing packet.
    pub(super) fn push(&mut self, packet: Vec<u8>, now: Instant) -> Vec<Vec<u8>> {
        let Some(parsed) = rtp::parse_packet(&packet) else {
            return self.flush_expired(now);
        };
        let (seq, ssrc) = (parsed.seq, parsed.ssrc);

        if self.ssrc != Some(ssrc) {
            if self.retired_ssrcs.contains(&ssrc) {
                // A straggler must not change the active sequence, pending
                // speech or restart probation. Still honor its deadlines.
                return self.flush_expired(now);
            }
            if let Some(previous) = self.ssrc {
                self.retired_ssrcs.insert(previous);
            }
            // A new source owns a new sequence space. Old-source packets
            // must not enter its decoder after the reset.
            self.pending.clear();
            self.deadline = None;
            self.restart_candidate = None;
            self.delivered.clear();
            self.delivery_order.clear();
            self.ssrc = Some(ssrc);
            self.expected = seq.wrapping_add(1);
            self.remember_delivery(&packet);
            return vec![packet];
        }

        // Apply the deadline before the new arrival: a packet that arrives
        // at or after expiry cannot fill a hole already skipped by the batch.
        let mut ready = self.flush_expired(now);
        let distance = seq.wrapping_sub(self.expected);
        if distance >= 0x8000 {
            // Already decoded media is evidence against a restart, even if
            // several old packets arrive consecutively. Reject it before it
            // can seed or confirm probation, preserving the active batch.
            if self
                .delivered
                .contains_key(&(seq, Self::media_identity(&parsed)))
            {
                self.restart_candidate = None;
                return ready;
            }
            // A same-SSRC source can restart its sequence far behind us.
            // Confirm it with a second consecutive packet before either can
            // reach the decoder. Check confirmation before the recent-history
            // guard: the second packet may cross that guard's boundary.
            match self.restart_candidate.take() {
                Some(candidate) if seq == candidate.seq.wrapping_add(1) => {
                    self.pending.clear();
                    self.deadline = None;
                    self.expected = seq.wrapping_add(1);
                    self.remember_delivery(&candidate.packet);
                    self.remember_delivery(&packet);
                    ready.extend([candidate.packet, packet]);
                }
                Some(candidate) if seq == candidate.seq => {
                    // Duplicates neither confirm nor extend probation.
                    self.restart_candidate = Some(candidate);
                }
                _ if self.expected.wrapping_sub(seq) > RECENT_SEQUENCE_HISTORY => {
                    self.restart_candidate = Some(RestartCandidate {
                        seq,
                        packet,
                        since: now,
                    });
                }
                _ => {} // Recently delivered/skipped packets remain too late.
            }
            return ready;
        }
        // Traffic in the current sequence space invalidates an isolated
        // restart candidate, including packets queued behind a live gap.
        self.restart_candidate = None;
        if distance == 0 {
            self.expected = seq.wrapping_add(1);
            self.remember_delivery(&packet);
            ready.push(packet);
            ready.extend(self.release_pending());
        } else {
            self.pending.entry(seq).or_insert(packet);
            // More future packets and duplicates cannot extend this wait.
            self.deadline.get_or_insert(now + self.max_wait);
            ready.extend(self.flush_expired(now));
        }
        ready
    }

    /// Allows the receive loop to release a batch even when no more packets
    /// arrive. The caller must wake at `deadline()` rather than await traffic.
    pub(super) fn flush_expired(&mut self, now: Instant) -> Vec<Vec<u8>> {
        if self.restart_candidate.as_ref().is_some_and(|candidate| {
            now.saturating_duration_since(candidate.since) >= RESTART_CONFIRMATION_WINDOW
        }) {
            self.restart_candidate = None;
        }
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.release_pending()
        } else {
            Vec::new()
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Auxiliary payloads can fill gaps only in this source's sequence space.
    pub(super) fn source_ssrc(&self) -> Option<u32> {
        self.ssrc
    }

    fn media_identity(parsed: &rtp::ParsedPacket<'_>) -> u64 {
        // Fingerprint media rather than the complete wire packet: extensions,
        // padding and the marker bit may change on a retransmission. Include
        // the payload so a real restart can reuse both counters for new audio.
        let mut hash = DefaultHasher::new();
        (parsed.timestamp, parsed.payload_type, parsed.payload).hash(&mut hash);
        hash.finish()
    }

    fn remember_delivery(&mut self, packet: &[u8]) {
        if let Some(parsed) = rtp::parse_packet(packet) {
            let identity = (parsed.seq, Self::media_identity(&parsed));
            *self.delivered.entry(identity).or_default() += 1;
            self.delivery_order.push_back(identity);
            if self.delivery_order.len() > DELIVERED_PACKET_HISTORY {
                if let Some(oldest) = self.delivery_order.pop_front() {
                    if let Some(count) = self.delivered.get_mut(&oldest) {
                        *count -= 1;
                        if *count == 0 {
                            self.delivered.remove(&oldest);
                        }
                    }
                }
            }
        }
    }

    fn release_pending(&mut self) -> Vec<Vec<u8>> {
        // Delivering the current run is progress too: an older probe must not
        // survive a timeout flush and later rewind the stream.
        self.restart_candidate = None;
        self.deadline = None;
        let mut packets: Vec<_> = std::mem::take(&mut self.pending).into_iter().collect();
        // Numeric sorting alone would put 0 before 65535 at sequence wrap.
        packets.sort_unstable_by_key(|(seq, _)| seq.wrapping_sub(self.expected));
        if let Some((last, _)) = packets.last() {
            self.expected = last.wrapping_add(1);
        }
        packets
            .into_iter()
            .map(|(_, packet)| {
                self.remember_delivery(&packet);
                packet
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(40);

    fn packet(seq: u16) -> Vec<u8> {
        rtp::build_packet(seq, seq as u32 * 160, 42, 96, &[0xf0, 0x04, 0x01])
    }

    fn seqs(packets: Vec<Vec<u8>>) -> Vec<u16> {
        packets
            .iter()
            .map(|packet| rtp::parse_packet(packet).unwrap().seq)
            .collect()
    }

    #[test]
    fn in_order_packets_are_immediate_and_unchanged() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        for seq in [10, 11, 12] {
            let original = packet(seq);
            assert_eq!(buffer.push(original.clone(), now), vec![original]);
            assert_eq!(buffer.deadline(), None);
        }
    }

    #[test]
    fn same_ssrc_sequence_restart_recovers_after_two_consecutive_packets() {
        for wait in [WAIT, Duration::ZERO] {
            let now = Instant::now();
            let mut buffer = RtpReorderBuffer::new(wait);
            assert_eq!(seqs(buffer.push(packet(10_000), now)), [10_000]);
            let first = packet(100);
            let second = packet(101);
            assert!(buffer.push(first.clone(), now).is_empty());
            assert_eq!(
                buffer.push(second.clone(), now + Duration::from_millis(20)),
                [first, second]
            );
            assert_eq!(
                seqs(buffer.push(packet(102), now + Duration::from_millis(40))),
                [102]
            );
            assert_eq!(buffer.deadline(), None);
        }
    }

    #[test]
    fn recent_late_packets_cannot_confirm_a_restart() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        for seq in [9_998, 9_999, 10_000] {
            assert!(buffer.push(packet(seq), now).is_empty());
        }
        assert_eq!(seqs(buffer.push(packet(10_001), now)), [10_001]);
    }

    #[test]
    fn consecutive_delivered_duplicates_preserve_the_active_batch_and_deadline() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        for seq in 9_900..=10_000 {
            assert_eq!(seqs(buffer.push(packet(seq), now)), [seq]);
        }
        assert!(buffer.push(packet(10_003), now).is_empty());
        for seq in [9_900, 9_901] {
            assert!(buffer
                .push(packet(seq), now + Duration::from_millis(10))
                .is_empty());
            assert_eq!(buffer.deadline(), Some(now + WAIT));
        }
        assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [10_003]);
        assert_eq!(seqs(buffer.push(packet(10_004), now + WAIT)), [10_004]);
    }

    #[test]
    fn a_delivered_duplicate_cannot_confirm_and_cancels_restart_probation() {
        let now = Instant::now();
        for candidate in [9_900, 9_899] {
            let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
            buffer.push(packet(9_901), now);
            buffer.push(packet(10_000), now);
            assert!(buffer.push(packet(candidate), now).is_empty());
            assert!(buffer.push(packet(9_901), now).is_empty());
            // A duplicate also cancels a nonconsecutive candidate, so its
            // successor cannot complete probation after that duplicate.
            assert!(buffer.push(packet(candidate + 1), now).is_empty());
            assert_eq!(seqs(buffer.push(packet(10_001), now)), [10_001]);
        }
    }

    #[test]
    fn genuine_restart_can_reuse_sequences_with_new_timestamps_or_payloads() {
        let now = Instant::now();
        for change_timestamp in [true, false] {
            let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
            buffer.push(packet(100), now);
            buffer.push(packet(101), now);
            buffer.push(packet(10_000), now);
            let restarted: Vec<_> = [100, 101]
                .into_iter()
                .map(|seq| {
                    if change_timestamp {
                        rtp::build_packet(seq, 0, 42, 96, &[0xf0, 0x04, 0x01])
                    } else {
                        // Both original counters may reset while the newly
                        // encoded speech differs from the earlier frames.
                        rtp::build_packet(seq, u32::from(seq) * 160, 42, 96, &[0xf0, 4, 2])
                    }
                })
                .collect();
            assert!(buffer.push(restarted[0].clone(), now).is_empty());
            assert_eq!(buffer.push(restarted[1].clone(), now), restarted);
        }
    }

    #[test]
    fn delivered_history_includes_batches_released_on_a_gap_or_timeout() {
        let now = Instant::now();
        for fill_gap in [false, true] {
            let mut buffer = RtpReorderBuffer::new(WAIT);
            buffer.push(packet(1), now);
            buffer.push(packet(100), now);
            buffer.push(packet(101), now);
            if fill_gap {
                assert_eq!(seqs(buffer.push(packet(2), now)), [2, 100, 101]);
            } else {
                assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [100, 101]);
            }
            buffer.push(packet(10_000), now + WAIT);
            assert_eq!(seqs(buffer.flush_expired(now + WAIT * 2)), [10_000]);
            assert!(buffer.push(packet(100), now + WAIT * 2).is_empty());
            assert!(buffer.push(packet(101), now + WAIT * 2).is_empty());
            assert_eq!(seqs(buffer.push(packet(10_001), now + WAIT * 2)), [10_001]);
        }
    }

    #[test]
    fn delivered_history_survives_a_same_ssrc_restart() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        buffer.push(packet(9_900), now);
        buffer.push(packet(9_901), now);
        buffer.push(packet(10_000), now);
        buffer.push(packet(100), now);
        assert_eq!(seqs(buffer.push(packet(101), now)), [100, 101]);
        buffer.push(packet(10_000), now);
        for seq in [9_900, 9_901, 100, 101] {
            assert!(buffer.push(packet(seq), now).is_empty());
        }
        assert_eq!(seqs(buffer.push(packet(10_001), now)), [10_001]);
    }

    #[test]
    fn old_media_stays_rejected_after_a_restart_reuses_its_sequence_numbers() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        for seq in 9_900..=10_000 {
            buffer.push(packet(seq), now);
        }
        let restarted = |seq| rtp::build_packet(seq, 0, 42, 96, &[0xf0, 4, 2]);
        assert!(buffer.push(restarted(9_900), now).is_empty());
        assert_eq!(seqs(buffer.push(restarted(9_901), now)), [9_900, 9_901]);
        for seq in 9_902..=10_000 {
            buffer.push(restarted(seq), now);
        }
        buffer.push(restarted(10_003), now);
        for replay in [
            packet(9_900),
            packet(9_901),
            restarted(9_900),
            restarted(9_901),
        ] {
            assert!(buffer.push(replay, now).is_empty());
            assert_eq!(buffer.deadline(), Some(now + WAIT));
        }
        assert_eq!(
            buffer.push(restarted(10_001), now),
            [restarted(10_001), restarted(10_003)]
        );
    }

    #[test]
    fn delivered_history_is_scoped_to_the_active_ssrc() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        buffer.push(packet(9_900), now);
        buffer.push(packet(9_901), now);
        let new_source =
            |seq| rtp::build_packet(seq, u32::from(seq) * 160, 99, 96, &[0xf0, 0x04, 0x01]);
        buffer.push(new_source(10_000), now);
        assert!(buffer.push(new_source(9_900), now).is_empty());
        assert_eq!(seqs(buffer.push(new_source(9_901), now)), [9_900, 9_901]);
    }

    #[test]
    fn sequence_wrap_allows_an_expected_packet_with_a_reused_media_identity() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        for seq in [0, 32_767, 65_534, 65_535, 0, 1] {
            assert_eq!(seqs(buffer.push(packet(seq), now)), [seq]);
        }
    }

    #[test]
    fn history_is_bounded_and_reused_identities_remain_known_after_eviction() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        for seq in 0..=u16::MAX {
            assert_eq!(seqs(buffer.push(packet(seq), now)), [seq]);
        }
        // Reuse exact identities as expected packets. Evicting their old
        // occurrences must not forget the newly delivered occurrences too.
        for seq in 0..=200 {
            assert_eq!(seqs(buffer.push(packet(seq), now)), [seq]);
        }
        assert_eq!(buffer.delivery_order.len(), DELIVERED_PACKET_HISTORY);
        assert!(buffer.delivered.len() <= DELIVERED_PACKET_HISTORY);
        assert!(buffer.push(packet(0), now).is_empty());
        assert!(buffer.push(packet(1), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(201), now)), [201]);
    }

    #[test]
    fn changed_marker_extension_or_padding_cannot_disguise_delivered_media() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        buffer.push(packet(9_900), now);
        buffer.push(packet(9_901), now);
        buffer.push(packet(10_000), now);
        for seq in [9_900, 9_901] {
            let mut replay = packet(seq);
            replay[0] |= 0x30; // RTP header extension and padding.
            replay[1] |= 0x80; // Different marker bit, same media.
            replay.splice(12..12, [0xbe, 0xde, 0, 1, 0, 0, 0, 0]);
            replay.extend([0, 2]);
            assert!(buffer.push(replay, now).is_empty());
        }
        assert_eq!(seqs(buffer.push(packet(10_001), now)), [10_001]);
    }

    #[test]
    fn current_run_traffic_cancels_an_isolated_restart_candidate() {
        let now = Instant::now();
        // A recent duplicate, the expected packet, and a future packet that
        // waits behind a gap all show activity in the current sequence run.
        for current_seq in [10_000, 10_001, 10_002] {
            let mut buffer = RtpReorderBuffer::new(WAIT);
            buffer.push(packet(10_000), now);
            assert!(buffer.push(packet(100), now).is_empty());
            let ready = seqs(buffer.push(packet(current_seq), now));
            if current_seq == 10_001 {
                assert_eq!(ready, [10_001]);
            } else {
                assert!(ready.is_empty());
            }
            assert!(buffer
                .push(packet(101), now + Duration::from_millis(1))
                .is_empty());
        }
    }

    #[test]
    fn nonconsecutive_old_packets_need_a_new_consecutive_pair() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        assert!(buffer.push(packet(100), now).is_empty());
        assert!(buffer.push(packet(102), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(103), now)), [102, 103]);
    }

    #[test]
    fn confirmed_restart_discards_the_previous_runs_pending_batch() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        buffer.push(packet(10_003), now);
        assert_eq!(buffer.deadline(), Some(now + WAIT));
        assert!(buffer.push(packet(100), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(101), now)), [100, 101]);
        assert_eq!(buffer.deadline(), None);
        assert!(buffer.flush_expired(now + WAIT).is_empty());
        assert_eq!(seqs(buffer.push(packet(102), now + WAIT)), [102]);
    }

    #[test]
    fn timeout_delivery_cancels_a_restart_candidate() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        buffer.push(packet(10_003), now);
        assert!(buffer.push(packet(100), now).is_empty());
        assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [10_003]);
        assert!(buffer.push(packet(101), now + WAIT).is_empty());
        assert_eq!(seqs(buffer.push(packet(102), now + WAIT)), [101, 102]);
    }

    #[test]
    fn duplicate_candidates_do_not_extend_the_confirmation_window() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        assert!(buffer.push(packet(100), now).is_empty());
        assert!(buffer
            .push(packet(100), now + Duration::from_millis(900))
            .is_empty());
        // At the exact deadline the original candidate has expired, even
        // though a duplicate was seen only 100ms ago.
        assert!(buffer
            .push(packet(101), now + RESTART_CONFIRMATION_WINDOW)
            .is_empty());
        assert_eq!(
            seqs(buffer.push(packet(102), now + RESTART_CONFIRMATION_WINDOW)),
            [101, 102]
        );
    }

    #[test]
    fn restart_confirmation_can_cross_the_recent_history_boundary() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        assert!(buffer.push(packet(9_900), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(9_901), now)), [9_900, 9_901]);
        assert_eq!(seqs(buffer.push(packet(9_902), now)), [9_902]);
    }

    #[test]
    fn restart_confirmation_handles_sequence_rollover() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        assert!(buffer.push(packet(u16::MAX), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(0), now)), [u16::MAX, 0]);
        assert_eq!(seqs(buffer.push(packet(1), now)), [1]);
    }

    #[test]
    fn ssrc_change_clears_restart_probation() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10_000), now);
        assert!(buffer.push(packet(100), now).is_empty());
        let new_source = |seq| rtp::build_packet(seq, 7, 99, 96, &[]);
        let first = new_source(20_000);
        assert_eq!(buffer.push(first.clone(), now), [first]);
        assert!(buffer.push(new_source(101), now).is_empty());
        assert_eq!(
            buffer.push(new_source(102), now),
            [new_source(101), new_source(102)]
        );
    }

    #[test]
    fn expected_packet_releases_permutation_in_order() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        assert_eq!(seqs(buffer.push(packet(10), now)), [10]);
        assert!(buffer.push(packet(14), now).is_empty());
        assert!(buffer.push(packet(12), now).is_empty());
        assert!(buffer.push(packet(13), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(11), now)), [11, 12, 13, 14]);
        assert_eq!(buffer.deadline(), None);
    }

    #[test]
    fn expected_packet_releases_all_buffered_packets_and_skips_other_holes() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        buffer.push(packet(15), now);
        buffer.push(packet(12), now);
        assert_eq!(seqs(buffer.push(packet(11), now)), [11, 12, 15]);
        assert!(buffer.push(packet(13), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(16), now)), [16]);
    }

    #[test]
    fn timeout_releases_all_packets_without_another_arrival() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        buffer.push(packet(15), now);
        buffer.push(packet(12), now + Duration::from_millis(10));
        assert_eq!(buffer.deadline(), Some(now + WAIT));
        assert!(buffer
            .flush_expired(now + WAIT - Duration::from_nanos(1))
            .is_empty());
        assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [12, 15]);
        assert_eq!(buffer.deadline(), None);
        assert!(buffer.push(packet(11), now + WAIT).is_empty());
        assert_eq!(seqs(buffer.push(packet(16), now + WAIT)), [16]);
    }

    #[test]
    fn arrival_at_deadline_cannot_recover_the_missing_packet() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        buffer.push(packet(12), now);
        assert_eq!(seqs(buffer.push(packet(11), now + WAIT)), [12]);
    }

    #[test]
    fn duplicate_and_late_packets_are_discarded_without_extending_wait() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        assert!(buffer.push(packet(10), now).is_empty());
        buffer.push(packet(12), now);
        assert!(buffer
            .push(packet(12), now + Duration::from_millis(20))
            .is_empty());
        assert_eq!(buffer.deadline(), Some(now + WAIT));
        assert_eq!(
            seqs(buffer.push(packet(11), now + Duration::from_millis(30))),
            [11, 12]
        );
        assert!(buffer
            .push(packet(12), now + Duration::from_millis(31))
            .is_empty());
    }

    #[test]
    fn sequence_wrap_preserves_modular_order() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        assert_eq!(seqs(buffer.push(packet(65533), now)), [65533]);
        buffer.push(packet(1), now);
        buffer.push(packet(0), now);
        buffer.push(packet(65535), now);
        assert_eq!(seqs(buffer.push(packet(65534), now)), [65534, 65535, 0, 1]);
        assert!(buffer.push(packet(65535), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(2), now)), [2]);
    }

    #[test]
    fn timeout_at_sequence_wrap_preserves_modular_order() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(65533), now);
        buffer.push(packet(1), now);
        buffer.push(packet(65535), now);
        assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [65535, 1]);
        assert_eq!(seqs(buffer.push(packet(2), now + WAIT)), [2]);
    }

    #[test]
    fn ssrc_change_resets_sequence_and_discards_pending_source() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(100), now);
        buffer.push(packet(102), now);
        let new_source = rtp::build_packet(1, 7, 99, 96, &[0xf0, 0x04, 0x01]);
        assert_eq!(buffer.push(new_source.clone(), now), [new_source]);
        assert_eq!(buffer.deadline(), None);
        assert!(buffer.flush_expired(now + WAIT).is_empty());
        assert_eq!(
            seqs(buffer.push(rtp::build_packet(2, 167, 99, 96, &[]), now)),
            [2]
        );
    }

    #[test]
    fn a_retired_ssrc_cannot_discard_the_active_sources_pending_packets() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(100), now);
        let new_source = |seq| rtp::build_packet(seq, u32::from(seq) * 160, 99, 96, &[]);
        assert_eq!(buffer.push(new_source(10), now), [new_source(10)]);
        assert!(buffer.push(new_source(12), now).is_empty());
        assert!(buffer
            .push(packet(101), now + Duration::from_millis(10))
            .is_empty());
        assert_eq!(buffer.deadline(), Some(now + WAIT));
        assert_eq!(
            buffer.push(new_source(11), now + Duration::from_millis(20)),
            [new_source(11), new_source(12)]
        );
        assert_eq!(buffer.push(new_source(13), now + WAIT), [new_source(13)]);
    }

    #[test]
    fn a_retired_ssrc_still_allows_the_active_sources_deadline_to_expire() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(100), now);
        let new_source = |seq| rtp::build_packet(seq, u32::from(seq) * 160, 99, 96, &[]);
        buffer.push(new_source(10), now);
        buffer.push(new_source(12), now);
        assert_eq!(buffer.push(packet(101), now + WAIT), [new_source(12)]);
        assert_eq!(buffer.deadline(), None);
        assert_eq!(buffer.push(new_source(13), now + WAIT), [new_source(13)]);
        assert!(buffer.push(packet(102), now + WAIT).is_empty());
    }

    #[test]
    fn all_retired_ssrcs_remain_rejected_after_multiple_source_switches() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        let from_source = |ssrc, seq| rtp::build_packet(seq, 160, ssrc, 96, &[]);
        for ssrc in 1..=32 {
            let original = from_source(ssrc, 1);
            assert_eq!(buffer.push(original.clone(), now), [original]);
        }
        for ssrc in 1..32 {
            assert!(buffer.push(from_source(ssrc, 2), now).is_empty());
        }
        assert_eq!(buffer.push(from_source(32, 2), now), [from_source(32, 2)]);
    }

    #[test]
    fn a_retired_ssrc_preserves_active_restart_probation_and_stays_retired() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(1), now);
        let new_source = |seq| rtp::build_packet(seq, u32::from(seq) * 160, 99, 96, &[]);
        buffer.push(new_source(10_000), now);
        assert!(buffer.push(new_source(100), now).is_empty());
        assert!(buffer
            .push(packet(2), now + Duration::from_millis(10))
            .is_empty());
        assert_eq!(
            buffer.push(new_source(101), now + Duration::from_millis(20)),
            [new_source(100), new_source(101)]
        );
        // Restarting the current sequence does not resurrect retired sources.
        assert!(buffer.push(packet(3), now + WAIT).is_empty());
        assert_eq!(buffer.push(new_source(102), now + WAIT), [new_source(102)]);
    }

    #[test]
    fn a_retired_ssrc_stays_rejected_with_waiting_disabled() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        buffer.push(packet(1), now);
        let new_source = |seq| rtp::build_packet(seq, u32::from(seq) * 160, 99, 96, &[]);
        assert_eq!(buffer.push(new_source(10), now), [new_source(10)]);
        assert!(buffer.push(packet(2), now).is_empty());
        assert_eq!(buffer.push(new_source(11), now), [new_source(11)]);
    }

    #[test]
    fn zero_wait_skips_missing_packets_immediately() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        assert_eq!(seqs(buffer.push(packet(10), now)), [10]);
        assert_eq!(seqs(buffer.push(packet(12), now)), [12]);
        assert!(buffer.push(packet(11), now).is_empty());
        assert_eq!(buffer.deadline(), None);
    }

    #[test]
    fn invalid_rtp_cannot_reset_stream_or_block_timeout() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        assert!(buffer.push(vec![0; 12], now).is_empty());
        buffer.push(packet(10), now);
        buffer.push(packet(12), now);
        assert_eq!(seqs(buffer.push(vec![0; 12], now + WAIT)), [12]);
        assert_eq!(seqs(buffer.push(packet(13), now + WAIT)), [13]);
    }
}
