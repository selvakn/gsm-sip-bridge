//! Container orchestration, moved out of `docker/entrypoint.sh`
//! (specs/021-entrypoint-supervise-rust). Decision logic (what to render, what
//! order to tear things down in, how a line's tunnel recovers) lives here as
//! plain Rust, tested without hardware; [`runner::CommandRunner`] is the one
//! seam through which it ever touches a real process or file.

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
