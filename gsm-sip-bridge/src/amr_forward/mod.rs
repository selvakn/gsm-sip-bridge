//! AMR passthrough (#111 option 2a, `specs/120-amr-passthrough-forwarder`):
//! forward an inbound call's AMR frames between the carrier and an AMR-capable
//! client without decoding them. Everything here is independent of PJSIP so
//! Agent A and Agent B share it and it is testable on its own.
//!
//! - [`params`] — what one leg negotiated, the compatibility rule (FR-003) and
//!   the recorded outcome ([`MediaPath`]).
//! - [`repack`] — octet-aligned ↔ bandwidth-efficient conversion that keeps
//!   every frame type and the codec mode request.
//! - [`relay`] — the socket forwarder (payload-type relabel, direction gate,
//!   counters).

pub mod params;
pub mod relay;
pub mod repack;

pub use params::{compatible, AmrLegParams, MediaPath, ModeSetPolicy, Reason, SdpLeg};
