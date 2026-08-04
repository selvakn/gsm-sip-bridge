//! Responder for the control socket while the circuit-switched path is off
//! (specs/026-disable-circuit-switched).
//!
//! `CardPool::run` is normally the sole consumer of the control-command
//! channel and the only thing that ever replies to it. With the pool not
//! running, this task takes its place so a card command gets a clear
//! "disabled" answer instead of an ambiguous generic one — `control::server`
//! already replies with `"daemon shutting down"`/`"no response from daemon"`
//! if the channel is simply dropped with nobody on the other end, but that
//! wording doesn't distinguish "deliberately disabled" from a real fault,
//! and doesn't name the flag (FR-019, FR-020).

use crate::control::protocol::{ControlCmd, ControlResp};
use crate::modules::ControlCmdReceiver;

/// Message every card-targeting command is refused with while `[cs].enabled`
/// is false.
pub const DISABLED_REASON: &str =
    "circuit-switched path is disabled ([cs].enabled = false) — no cards are managed";

/// Drains `control_rx` for the lifetime of the process, answering every
/// card-targeting command with [`DISABLED_REASON`]. Spawned in place of
/// `CardPool::run` when `[cs].enabled` is false, so the control socket's
/// sole consumer invariant holds in both states.
pub async fn run(mut control_rx: ControlCmdReceiver) {
    while let Some((cmd, resp_tx)) = control_rx.recv().await {
        let resp = match cmd {
            ControlCmd::CardRestart { .. }
            | ControlCmd::SetMode { .. }
            | ControlCmd::GetMode { .. }
            | ControlCmd::ListSlots => ControlResp::err(DISABLED_REASON),
            // Never actually reaches here — `control::server::handle_connection`
            // routes `Observe` straight to `metrics::ingest::apply_report`
            // before a command ever reaches this channel (see
            // `ControlCmd::Observe`'s own doc comment). Handled rather than
            // left unreachable so a future routing change fails safe instead
            // of panicking.
            ControlCmd::Observe { .. } => ControlResp::ok(),
        };
        let _ = resp_tx.send(resp);
    }
}
