//! The embedded SIP registrar: IP phones REGISTER here and the bridge INVITEs
//! whichever one `[sip_server].ring_aor` names (spec 024).
//!
//! Pure safe Rust on its own UDP socket, deliberately *not* a PJSIP module
//! sharing pjsua's transport. A module would be nicer on the wire — the phone
//! would see INVITEs from the same port it registered to — but it would live
//! entirely behind the `pjsip-linked` feature, which neither `make test`,
//! `make lint`, nor CI ever enables. An authentication subsystem that is never
//! compiled or run in CI fails both of the constitution's non-negotiable
//! principles while appearing to satisfy them. See research.md R-001/R-002.

pub mod auth;
pub mod bindings;

pub use bindings::{Binding, BindingStore};
