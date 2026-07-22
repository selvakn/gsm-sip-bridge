//! Media condition measurement (specs/016-volte-calls, US2/US3).
//!
//! Sample counts alone cannot tell a clean call from one that arrived in
//! bursts with a third missing — both yield similar totals. This module
//! derives the difference from what is already on the wire: `rtp::parse_packet`
//! extracts sequence numbers and timestamps today and then discards them.
//!
//! Everything here is pure and clock-injectable, so it is unit-testable
//! without a modem, a SIM or a carrier — unusually for this project, and the
//! reason the plan front-loads it.

use std::time::Duration;

/// Default proportion of sent audio that must come back before a direction
/// counts as working (specs/016-volte-calls FR-016).
///
/// A **ratio**, deliberately, not an absolute floor. An absolute floor either
/// passes a call that received a handful of packets across thirty seconds, or
/// fails a short call that was fine. A ratio stays correct whatever the call's
/// length — and a quiet answering party still produces audio frames, so this
/// separates "nothing is reaching us" from "they said nothing", which a
/// loudness measurement could not.
pub const DEFAULT_ONE_WAY_THRESHOLD_PERCENT: u8 = 10;

/// Which directions actually carried audio.
///
/// `SendOnly` vs `ReceiveOnly` is the distinction that turned the previous
/// one-way-audio incident (`docs/incidents/2026-07-15-vowifi-oneway-audio.md`)
/// from a mystery into a diagnosis. It must never be collapsed into a single
/// "no audio" state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectionVerdict {
    /// Audio flowed both ways. The only verdict that counts as success.
    BothWays,
    /// We sent; little or nothing came back. The carrier is not sending, or
    /// we cannot decode what it sends.
    SendOnly,
    /// Audio arrived; little or nothing of ours got out.
    ReceiveOnly,
    /// Nothing either way — media never established.
    Neither,
}

impl DirectionVerdict {
    /// True only for `BothWays`. An answered call with any other verdict is a
    /// **failure**, not a success (FR-016) — the previous incident was painful
    /// precisely because a broken call looked like a working one.
    pub fn is_success(self) -> bool {
        self == DirectionVerdict::BothWays
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DirectionVerdict::BothWays => "both-ways",
            DirectionVerdict::SendOnly => "send-only",
            DirectionVerdict::ReceiveOnly => "receive-only",
            DirectionVerdict::Neither => "neither",
        }
    }

    /// Operator-facing explanation of what the verdict points at.
    pub fn diagnosis(self) -> &'static str {
        match self {
            DirectionVerdict::BothWays => "audio flowed in both directions",
            DirectionVerdict::SendOnly => {
                "we transmitted but little or nothing came back — the carrier is not sending, \
                 or we cannot decode what it sends"
            }
            DirectionVerdict::ReceiveOnly => {
                "audio arrived but little or nothing of ours got out — suspect our transmit \
                 path, or the network dropping our uplink"
            }
            DirectionVerdict::Neither => "no audio in either direction — media never established",
        }
    }
}

/// Judges each direction independently against the threshold (FR-028).
///
/// `reference` is what the *other* side of each comparison should have carried:
/// for a call, both directions are compared against the larger of the two, so a
/// direction is judged relative to what actually moved rather than against an
/// absolute expectation the call length would invalidate.
pub fn verdict(
    sent_samples: u64,
    received_samples: u64,
    threshold_percent: u8,
) -> DirectionVerdict {
    let reference = sent_samples.max(received_samples);
    if reference == 0 {
        return DirectionVerdict::Neither;
    }
    let ok = |n: u64| (n as u128) * 100 >= (reference as u128) * (threshold_percent as u128);
    match (ok(sent_samples), ok(received_samples)) {
        (true, true) => DirectionVerdict::BothWays,
        (true, false) => DirectionVerdict::SendOnly,
        (false, true) => DirectionVerdict::ReceiveOnly,
        (false, false) => DirectionVerdict::Neither,
    }
}

/// What the receive side observed in transit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiveStats {
    pub received_packets: u64,
    /// Derived from sequence gaps, not counted directly.
    pub lost_packets: u64,
    /// Arrived after a higher sequence number had already been seen.
    /// Counted separately from loss: a reordered packet is not a lost one,
    /// and treating it as loss overstates how bad the path is.
    pub reordered_packets: u64,
    /// Inter-arrival jitter, RFC 3550 §6.4.1.
    pub jitter: Duration,
}

impl ReceiveStats {
    /// Loss as a percentage of what should have arrived.
    pub fn loss_percent(&self) -> f64 {
        let expected = self.received_packets + self.lost_packets;
        if expected == 0 {
            return 0.0;
        }
        (self.lost_packets as f64) * 100.0 / (expected as f64)
    }
}

/// Tracks sequence continuity and arrival timing across a call.
///
/// Sequence numbers are 16-bit and wrap; this extends them internally so a
/// call lasting past 65535 packets (about 22 minutes at 20ms) does not read as
/// catastrophic loss the moment it wraps.
#[derive(Debug, Default)]
pub struct ReceiveTracker {
    base_extended: Option<u64>,
    highest_extended: u64,
    cycles: u64,
    last_seq: u16,
    received: u64,
    reordered: u64,
    /// RFC 3550 jitter, carried in RTP timestamp units.
    jitter_units: f64,
    last_transit: Option<i64>,
}

impl ReceiveTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one received packet.
    ///
    /// `arrival` is time since an arbitrary but fixed origin, and
    /// `rtp_timestamp`/`clock_rate` are the packet's own timing — the pair is
    /// what makes jitter measurable rather than guessed.
    pub fn on_packet(&mut self, seq: u16, rtp_timestamp: u32, arrival: Duration, clock_rate: u32) {
        let extended = match self.base_extended {
            None => {
                self.base_extended = Some(seq as u64);
                self.highest_extended = seq as u64;
                self.last_seq = seq;
                seq as u64
            }
            Some(_) => {
                // Wrap detection: a large backwards jump is a wrap forwards,
                // not 65000 packets of reordering.
                if seq < self.last_seq && self.last_seq.wrapping_sub(seq) > u16::MAX / 2 {
                    self.cycles += 1;
                }
                let extended = self.cycles * (u16::MAX as u64 + 1) + seq as u64;
                if extended > self.highest_extended {
                    self.highest_extended = extended;
                } else {
                    self.reordered += 1;
                }
                self.last_seq = seq;
                extended
            }
        };
        let _ = extended;
        self.received += 1;

        // RFC 3550 §6.4.1: D = (Rj - Ri) - (Sj - Si), J += (|D| - J) / 16.
        if clock_rate > 0 {
            let arrival_units = (arrival.as_secs_f64() * clock_rate as f64).round() as i64;
            let transit = arrival_units - rtp_timestamp as i64;
            if let Some(prev) = self.last_transit {
                let d = (transit - prev).abs() as f64;
                self.jitter_units += (d - self.jitter_units) / 16.0;
            }
            self.last_transit = Some(transit);
        }
    }

    /// Snapshot of what has been observed so far.
    pub fn stats(&self, clock_rate: u32) -> ReceiveStats {
        let expected = match self.base_extended {
            None => 0,
            Some(base) => self.highest_extended.saturating_sub(base) + 1,
        };
        ReceiveStats {
            received_packets: self.received,
            lost_packets: expected.saturating_sub(self.received),
            reordered_packets: self.reordered,
            jitter: if clock_rate > 0 {
                Duration::from_secs_f64(self.jitter_units / clock_rate as f64)
            } else {
                Duration::ZERO
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    // ---- direction verdict -------------------------------------------------

    #[test]
    fn a_clean_bidirectional_call_is_both_ways() {
        assert_eq!(verdict(48000, 47500, 10), DirectionVerdict::BothWays);
        assert!(verdict(48000, 47500, 10).is_success());
    }

    #[test]
    fn received_far_below_sent_is_send_only_and_a_failure() {
        let v = verdict(48000, 200, 10);

        assert_eq!(v, DirectionVerdict::SendOnly);
        assert!(
            !v.is_success(),
            "a one-way call must never count as success"
        );
        assert!(v.diagnosis().contains("carrier is not sending"));
    }

    #[test]
    fn sent_far_below_received_is_receive_only_and_a_failure() {
        let v = verdict(200, 48000, 10);

        assert_eq!(v, DirectionVerdict::ReceiveOnly);
        assert!(!v.is_success());
        assert!(v.diagnosis().contains("uplink"));
    }

    #[test]
    fn nothing_either_way_is_neither() {
        assert_eq!(verdict(0, 0, 10), DirectionVerdict::Neither);
    }

    #[test]
    fn the_threshold_boundary_is_inclusive() {
        // Exactly 10% of the reference must pass, so the documented default is
        // the boundary rather than something just inside it.
        assert_eq!(verdict(1000, 100, 10), DirectionVerdict::BothWays);
        assert_eq!(verdict(1000, 99, 10), DirectionVerdict::SendOnly);
    }

    #[test]
    fn a_short_proportionally_healthy_call_passes() {
        // Length-independence: 300 samples is tiny in absolute terms.
        assert_eq!(verdict(300, 290, 10), DirectionVerdict::BothWays);
    }

    #[test]
    fn a_long_call_with_a_tiny_absolute_receive_fails() {
        // The case an absolute floor would have wrongly passed: 30s sent,
        // ~40ms received.
        assert_eq!(verdict(480_000, 2_000, 10), DirectionVerdict::SendOnly);
    }

    #[test]
    fn verdict_does_not_overflow_on_large_counts() {
        assert_eq!(verdict(u64::MAX, u64::MAX, 10), DirectionVerdict::BothWays);
    }

    // ---- sequence tracking -------------------------------------------------

    #[test]
    fn a_clean_stream_reports_no_loss() {
        let mut t = ReceiveTracker::new();
        for i in 0..100u16 {
            t.on_packet(i, i as u32 * 320, ms(i as u64 * 20), 16000);
        }

        let s = t.stats(16000);

        assert_eq!(s.received_packets, 100);
        assert_eq!(s.lost_packets, 0);
        assert_eq!(s.reordered_packets, 0);
        assert_eq!(s.loss_percent(), 0.0);
    }

    #[test]
    fn a_sequence_gap_is_counted_as_loss() {
        let mut t = ReceiveTracker::new();
        for i in [0u16, 1, 2, 5, 6] {
            t.on_packet(i, i as u32 * 320, ms(i as u64 * 20), 16000);
        }

        let s = t.stats(16000);

        assert_eq!(s.received_packets, 5);
        assert_eq!(s.lost_packets, 2, "3 and 4 never arrived");
        assert_eq!(s.reordered_packets, 0);
    }

    #[test]
    fn out_of_order_arrival_is_reordering_not_loss() {
        // Treating reordering as loss overstates how bad the path is.
        let mut t = ReceiveTracker::new();
        for i in [0u16, 1, 3, 2, 4] {
            t.on_packet(i, i as u32 * 320, ms(i as u64 * 20), 16000);
        }

        let s = t.stats(16000);

        assert_eq!(s.received_packets, 5);
        assert_eq!(s.lost_packets, 0, "everything arrived, just not in order");
        assert_eq!(s.reordered_packets, 1);
    }

    #[test]
    fn sequence_wraparound_is_not_mistaken_for_massive_loss() {
        // A call past 65535 packets (~22 min at 20ms) must not read as
        // catastrophic loss the moment the counter wraps.
        let mut t = ReceiveTracker::new();
        for seq in [65533u16, 65534, 65535, 0, 1, 2] {
            t.on_packet(seq, 0, ms(0), 0);
        }

        let s = t.stats(0);

        assert_eq!(s.received_packets, 6);
        assert_eq!(s.lost_packets, 0, "wrap must not look like loss");
    }

    #[test]
    fn loss_percent_is_relative_to_what_should_have_arrived() {
        let mut t = ReceiveTracker::new();
        for i in [0u16, 1, 2, 3, 4, 5, 6, 7, 8, 19] {
            t.on_packet(i, 0, ms(0), 0);
        }

        let s = t.stats(0);

        assert_eq!(s.received_packets, 10);
        assert_eq!(s.lost_packets, 10);
        assert_eq!(s.loss_percent(), 50.0);
    }

    // ---- jitter ------------------------------------------------------------

    #[test]
    fn perfectly_paced_arrival_has_no_jitter() {
        let mut t = ReceiveTracker::new();
        // 20ms packets at 16kHz = 320 samples per packet, arriving exactly on time.
        for i in 0..50u16 {
            t.on_packet(i, i as u32 * 320, ms(i as u64 * 20), 16000);
        }

        assert_eq!(t.stats(16000).jitter, Duration::ZERO);
    }

    #[test]
    fn irregular_arrival_produces_jitter() {
        let mut t = ReceiveTracker::new();
        // Same packets, arriving with a wobble.
        let arrivals = [0u64, 25, 38, 65, 78, 105];
        for (i, &a) in arrivals.iter().enumerate() {
            t.on_packet(i as u16, i as u32 * 320, ms(a), 16000);
        }

        assert!(
            t.stats(16000).jitter > Duration::ZERO,
            "uneven arrival must register as jitter"
        );
    }

    #[test]
    fn an_empty_tracker_reports_nothing_rather_than_panicking() {
        let s = ReceiveTracker::new().stats(16000);

        assert_eq!(s, ReceiveStats::default());
        assert_eq!(s.loss_percent(), 0.0);
    }
}
