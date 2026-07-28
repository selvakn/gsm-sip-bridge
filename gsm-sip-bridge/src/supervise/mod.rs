//! Container orchestration, moved out of `docker/entrypoint.sh`
//! (specs/021-entrypoint-supervise-rust). Decision logic (what to render, what
//! order to tear things down in, how a line's tunnel recovers) lives here as
//! plain Rust, tested without hardware; [`runner::CommandRunner`] is the one
//! seam through which it ever touches a real process or file.
//!
//! # `docker/entrypoint.sh` no longer orchestrates anything
//!
//! It is a 28-line shim: check that the binary and config exist, then `exec
//! gsm-sip-bridge supervise`. Every reference to it in the rest of this
//! codebase is *provenance* — "this is a 1:1 port of the bash that used to do
//! X" — and is written in the past tense for that reason. If you are looking
//! for what actually happens at container start, it is here:
//!
//! | Concern | Module |
//! |---|---|
//! | Startup sequencing, per-line process supervision | [`orchestrate`] |
//! | The VoLTE half of the same | [`orchestrate_volte`] |
//! | Teardown ordering (the old `cleanup()` trap) | [`shutdown`] |
//! | strongSwan/vpcd config asset rendering | [`render`] |
//! | Per-line tunnel state machine and recovery | [`line_supervisor`] |
//! | SIM power-cycle recovery | [`sim_recovery`] |
//! | The circuit-switched daemon's respawn loop | [`daemon_supervisor`] |
//!
//! Container *health* checking lives in [`crate::commands::healthcheck`],
//! which is what `HEALTHCHECK` invokes.

pub mod daemon_supervisor;
pub mod engines;
pub mod epdg_iface;
pub mod line_supervisor;
pub mod orchestrate;
pub mod orchestrate_volte;
pub mod render;
pub mod runner;
pub mod shutdown;
pub mod sim_recovery;
pub mod vpcd;
