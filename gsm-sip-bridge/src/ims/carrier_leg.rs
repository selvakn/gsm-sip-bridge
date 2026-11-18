//! The carrier leg of a bridged call as its running media relay sees it, which
//! a carrier re-INVITE can change without the relay being rebuilt.
//!
//! A carrier that puts the far party on hold, or takes it off hold, sends an
//! in-dialog re-INVITE (RFC 3264 §8.4). Its offer may also restate the codec
//! the call is running under another payload type number, or with the other
//! AMR framing: T2's SBC did both on a live call (2026-11-18), moving AMR-WB
//! from payload type 96 octet-aligned to 102 bandwidth-efficient. Answering
//! it means that, from the moment the answer is sent, this side sends with
//! the labels the answer names and only in the direction it agreed to (RFC
//! 3264 §8.3.2, §6.1). The codec itself never changes here, so the relay's
//! decoder, encoder and resampler stay valid; only how the carrier leg's
//! packets are labelled and framed, and whether this side sends toward the
//! carrier at all, do.
//!
//! [`CarrierLeg`] is the one place both sides meet: `agent::reinvite` writes
//! it once the `200 OK` is sent, and each direction of the relay reads it for
//! every packet.

use super::sdp::{ChosenCodec, NegotiatedCodec};
use std::sync::{Arc, Mutex};

/// How the carrier leg's RTP is labelled for the codec the call runs: its
/// payload type number, AMR framing, and `telephone-event` payload type and
/// clock rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarrierLabels {
    pub payload_type: u8,
    /// RFC 4867 octet-aligned framing rather than bandwidth-efficient.
    /// Meaningless for PCMU.
    pub octet_aligned: bool,
    pub dtmf_payload_type: Option<u8>,
    /// The clock an event's duration counts in (RFC 4733 §2.3), as
    /// [`ChosenCodec::dtmf_clock_rate`]: a re-offer can move the event to
    /// another rate as well as another number. `Some` exactly when
    /// `dtmf_payload_type` is.
    pub dtmf_clock_rate: Option<u32>,
}

impl CarrierLabels {
    pub fn of(chosen: &ChosenCodec) -> Self {
        Self {
            payload_type: chosen.payload_type,
            octet_aligned: chosen.octet_aligned,
            dtmf_payload_type: chosen.dtmf_payload_type,
            dtmf_clock_rate: chosen.dtmf_clock_rate,
        }
    }

    /// The leg's `telephone-event` as `(payload type, clock rate)`, as
    /// [`ChosenCodec::telephone_event`].
    pub fn telephone_event(&self) -> Option<(u8, u32)> {
        self.dtmf_payload_type.zip(self.dtmf_clock_rate)
    }
}

/// What the relay is to do on the carrier leg right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarrierLegState {
    pub labels: CarrierLabels,
    /// Whether this side may send media toward the carrier. `false` while
    /// the latest SDP exchange has this side `recvonly` or `inactive` (a
    /// hold); what the carrier sends is still received and relayed.
    pub sending: bool,
    /// The audio payload type and AMR framing the carrier used before the
    /// latest relabel, which its packets may still carry for a while, as
    /// `(payload type, octet-aligned)`. `None` when there is none, and also
    /// once a later offer from the carrier gave that number another meaning:
    /// RFC 3264 §8.3.2 says a dynamic payload type keeps its meaning for the
    /// whole session, but T2's SBC moved 96 from AMR-WB to
    /// `telephone-event/8000` on a hold (2026-11-21), so a packet under the
    /// old number is read as the old audio only while nothing says otherwise.
    /// Only the direction from the carrier reads it
    /// ([`CarrierEnd::previous_audio`]).
    pub previous_audio: Option<(u8, bool)>,
}

/// Which label changes a relay can follow while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relabel {
    /// Any payload type, framing and `telephone-event` payload type for the
    /// same codec: the transcoding relay frames every packet itself.
    Any,
    /// Only the `telephone-event` payload type and clock rate: the
    /// pass-through relay forwards audio packets byte for byte, so the audio
    /// payload type the carrier uses must stay the one the telephone leg
    /// uses.
    DtmfOnly,
}

/// A running call's carrier leg, shared between the agent's dispatch loop and
/// its relay threads. See the module documentation.
#[derive(Debug)]
pub struct CarrierLeg {
    codec: NegotiatedCodec,
    relabel: Relabel,
    state: Mutex<CarrierLegState>,
}

impl CarrierLeg {
    /// The leg as the relay starts it: labelled as `chosen`, sending.
    pub fn new(chosen: &ChosenCodec, relabel: Relabel) -> Arc<Self> {
        Arc::new(Self {
            codec: chosen.codec,
            relabel,
            state: Mutex::new(CarrierLegState {
                labels: CarrierLabels::of(chosen),
                sending: true,
                previous_audio: None,
            }),
        })
    }

    /// The codec the relay was built for, which nothing here can change.
    pub fn codec(&self) -> NegotiatedCodec {
        self.codec
    }

    pub fn state(&self) -> CarrierLegState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the relay can carry on with `labels` instead of its current
    /// ones.
    pub fn can_follow(&self, labels: CarrierLabels) -> bool {
        match self.relabel {
            Relabel::Any => true,
            Relabel::DtmfOnly => {
                let current = self.state().labels;
                labels.payload_type == current.payload_type
                    && labels.octet_aligned == current.octet_aligned
            }
        }
    }

    /// Switches the relay to `state`. `false`, with nothing changed, when its
    /// labels are ones the relay cannot follow ([`Self::can_follow`]).
    pub fn set(&self, state: CarrierLegState) -> bool {
        if !self.can_follow(state.labels) {
            return false;
        }
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
        true
    }
}

/// Which end of one relay direction is the carrier leg.
#[derive(Debug, Clone)]
pub enum CarrierEnd {
    /// Packets come from the carrier (the downlink).
    Source(Arc<CarrierLeg>),
    /// Packets go to the carrier (the uplink), which a hold can stop.
    Destination(Arc<CarrierLeg>),
}

impl CarrierEnd {
    pub fn state(&self) -> CarrierLegState {
        match self {
            Self::Source(leg) | Self::Destination(leg) => leg.state(),
        }
    }

    /// Whether this direction may send now: always toward the telephone
    /// leg, and toward the carrier only while the leg is sending.
    pub fn may_send(&self, state: &CarrierLegState) -> bool {
        match self {
            Self::Source(_) => true,
            Self::Destination(_) => state.sending,
        }
    }

    /// The carrier's audio before the latest relabel, as
    /// [`CarrierLegState::previous_audio`], for the direction that receives
    /// from the carrier. The other receives from the telephone leg, whose
    /// labels never change.
    pub fn previous_audio(&self, state: &CarrierLegState) -> Option<(u8, bool)> {
        match self {
            Self::Source(_) => state.previous_audio,
            Self::Destination(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amr_wb(payload_type: u8, octet_aligned: bool, dtmf: Option<u8>) -> ChosenCodec {
        ChosenCodec {
            codec: NegotiatedCodec::AmrWb,
            payload_type,
            octet_aligned,
            dtmf_payload_type: dtmf,
            dtmf_clock_rate: dtmf.map(|_| 16000),
        }
    }

    #[test]
    fn a_new_leg_sends_with_the_answers_labels() {
        let leg = CarrierLeg::new(&amr_wb(96, true, Some(105)), Relabel::Any);
        assert_eq!(
            leg.state(),
            CarrierLegState {
                labels: CarrierLabels {
                    payload_type: 96,
                    octet_aligned: true,
                    dtmf_payload_type: Some(105),
                    dtmf_clock_rate: Some(16000),
                },
                sending: true,
                previous_audio: None,
            }
        );
        assert_eq!(leg.codec(), NegotiatedCodec::AmrWb);
    }

    #[test]
    fn the_transcoding_relay_follows_any_relabel() {
        let leg = CarrierLeg::new(&amr_wb(96, true, Some(105)), Relabel::Any);
        let held = CarrierLegState {
            labels: CarrierLabels::of(&amr_wb(102, false, Some(103))),
            sending: false,
            previous_audio: None,
        };
        assert!(leg.set(held));
        assert_eq!(leg.state(), held);
    }

    #[test]
    fn the_pass_through_relay_follows_dtmf_and_direction_only() {
        let pcmu = ChosenCodec {
            codec: NegotiatedCodec::Pcmu,
            payload_type: 0,
            octet_aligned: false,
            dtmf_payload_type: Some(101),
            dtmf_clock_rate: Some(8000),
        };
        let leg = CarrierLeg::new(&pcmu, Relabel::DtmfOnly);
        let start = leg.state();
        let moved_audio = CarrierLegState {
            labels: CarrierLabels {
                payload_type: 8,
                ..start.labels
            },
            sending: true,
            previous_audio: None,
        };
        assert!(!leg.set(moved_audio));
        assert_eq!(leg.state(), start, "refused, nothing changed");

        let held_new_dtmf = CarrierLegState {
            labels: CarrierLabels {
                dtmf_payload_type: Some(100),
                dtmf_clock_rate: Some(16000),
                ..start.labels
            },
            sending: false,
            previous_audio: None,
        };
        assert!(leg.set(held_new_dtmf));
        assert_eq!(leg.state(), held_new_dtmf);
    }

    /// Only the direction from the carrier reads the carrier's old audio
    /// number; the other receives from the telephone leg.
    #[test]
    fn only_the_direction_from_the_carrier_reads_the_old_audio_number() {
        let leg = CarrierLeg::new(&amr_wb(96, true, None), Relabel::Any);
        let relabelled = CarrierLegState {
            labels: CarrierLabels::of(&amr_wb(102, false, None)),
            sending: true,
            previous_audio: Some((96, true)),
        };
        assert!(leg.set(relabelled));
        let state = leg.state();
        assert_eq!(
            CarrierEnd::Source(leg.clone()).previous_audio(&state),
            Some((96, true))
        );
        assert_eq!(CarrierEnd::Destination(leg).previous_audio(&state), None);
    }

    #[test]
    fn only_the_direction_toward_the_carrier_is_held() {
        let leg = CarrierLeg::new(&amr_wb(96, true, None), Relabel::Any);
        let held = CarrierLegState {
            sending: false,
            ..leg.state()
        };
        leg.set(held);
        let state = leg.state();
        assert!(CarrierEnd::Source(leg.clone()).may_send(&state));
        assert!(!CarrierEnd::Destination(leg).may_send(&state));
    }
}
