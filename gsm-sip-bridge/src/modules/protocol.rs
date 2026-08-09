//! The two message types the card pool and its per-module workers exchange,
//! plus the control-socket channel aliases.
//!
//! Split out of `modules::mod` because both sides of the conversation
//! (`modules::pool`, which sends `ModuleCmd` and receives `BridgeEvent`, and
//! `modules::worker`, which does the reverse) need these, and neither owns
//! them. Keeping them in the parent would have forced one half to reach
//! through the other.

use crate::alerts;
use crate::control::protocol::{ControlCmd, ControlResp};
use crate::modules::at_commander::NetworkMode;
use crate::modules::restart_policy::RestartMode;
use tokio::sync::{mpsc, oneshot};

/// Worker -> pool. Sent from the blocking per-module loop over an unbounded
/// channel; handled on the pool's async side by `CardPool::handle_bridge_event`.
pub(crate) enum BridgeEvent {
    Ring {
        module_id: String,
        caller_id: String,
        audio_device: String,
    },
    Hangup {
        module_id: String,
    },
    SmsReceived {
        module_id: String,
        sender: String,
        body: String,
        received_at: String,
    },
    NetworkLost {
        module_id: String,
    },
    /// specs/022-discord-critical-alerts. Sent from the blocking per-module
    /// loop (which has no direct tokio `Handle`) so the async side can
    /// dispatch it via `Handle::current()`, mirroring `SmsReceived`.
    CriticalAlert(alerts::CriticalEvent),
}

pub type ControlCmdSender = mpsc::Sender<(ControlCmd, oneshot::Sender<ControlResp>)>;
pub type ControlCmdReceiver = mpsc::Receiver<(ControlCmd, oneshot::Sender<ControlResp>)>;

/// Pool -> worker. Delivered over a `crossbeam_channel` because the receiving
/// end lives on a blocking thread, not in the tokio runtime.
pub(crate) enum ModuleCmd {
    SetMode(NetworkMode, oneshot::Sender<Result<NetworkMode, String>>),
    Reboot(RestartMode),
    /// Dial an outbound call on this line (specs/025-outbound-calling).
    /// `Ok(())` means the modem accepted `ATD`, not that the call was
    /// answered — see `AtCommander::dial`'s own doc comment.
    Dial(String, oneshot::Sender<Result<(), String>>),
    /// Hang up a call this line just dialed, fire-and-forget (matches
    /// `Reboot`'s no-response shape). Exists for the narrow case where
    /// `ATD` already succeeded but accepting the SIP-side leg failed
    /// afterward (specs/025-outbound-calling review) — without this, the
    /// modem stays on a real, live call while `SlotState` reports it idle
    /// and eligible for a second dial.
    Hangup,
}
