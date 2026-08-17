//! Everything that talks to a Quectel modem over its AT serial port, plus the
//! circuit-switched card pool built on top.
//!
//! This module root used to hold the whole card pool inline (~2400 lines of
//! production code). It is now a facade: the pool and its per-module worker
//! live in the submodules below, split by concern.
//!
//! | Concern | Module |
//! |---|---|
//! | Serial AT transport | [`at_commander`] |
//! | USB/AT discovery, SIM probing, port quarantine | [`discovery`] |
//! | The async card pool that owns every slot | [`pool`] |
//! | One blocking AT worker per modem | [`worker`] |
//! | Messages between those two | [`protocol`] |
//! | A slot's runtime record | [`slot`] |
//! | Restart severity and post-restart backoff | [`restart_policy`] |
//! | The scheduled auto-restart FSM (pure) | [`scheduler`] |
//!
//! `CardPool` and the two `ControlCmd` channel aliases are re-exported at this
//! root because `commands::daemon` and `control::disabled` have always
//! referred to them as `modules::CardPool` / `modules::ControlCmdSender` —
//! same precedent as `volte::read_access_network_info`, which kept its
//! historical path after its implementation moved into a submodule.

pub mod at_commander;
pub(crate) mod at_worker;
pub mod audio_pipeline;
pub mod beep;
pub mod card;
pub mod discovery;
pub mod modem_lock;
pub mod pcsc_card;
pub mod pcsc_list;
pub mod scheduler;
pub mod usim;

mod pool;
mod protocol;
mod restart_policy;
mod slot;
mod worker;

pub use pool::CardPool;
pub use protocol::{ControlCmdReceiver, ControlCmdSender};
