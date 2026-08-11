//! The three things that arrive from outside the pool's own timers: control
//! socket commands, events from the per-module workers, and outbound call
//! requests from the SIP side.
//!
//! Split from `pool::mod` because all three are pure dispatch — one big
//! `match` each, no shared state beyond the slot map — and together they were
//! nearly a third of `CardPool`'s original `impl`.

use super::CardPool;
use crate::alerts;
use crate::control::protocol::{ControlCmd, ControlResp, SlotInfo};
use crate::metrics;
use crate::modules::at_commander::{NetworkMode, NetworkType};
use crate::modules::protocol::{BridgeEvent, ModuleCmd};
use crate::modules::restart_policy::{retry_delay_for, RestartMode};
use crate::modules::scheduler::{AttemptType, CycleOutcome, Outcome};
use crate::modules::slot::{backoff_delay, claim_idle_cs_slot, LifecycleState, SlotState};
use crate::sms;
use crate::store::StoreCommand;
use pjsua_safe::Call;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinSet;

impl CardPool {
    /// Handles one `SipBridge::poll_outbound_request` result: validate,
    /// select an idle CS line, dial it, and accept or refuse the SIP call
    /// accordingly (specs/025-outbound-calling, US1).
    ///
    /// Teardown needs no new code here: `ModuleCmd::Dial`
    /// (`apply_dial_cmd`) already sets `card.state = Answering` on success,
    /// which is exactly the state the existing SIP-peer-disconnect check in
    /// the module worker and the existing `BridgeEvent::Hangup` handling
    /// (both written for the inbound-call direction) already watch —
    /// reused here unmodified in the other direction.
    pub(super) async fn handle_outbound_request(
        &mut self,
        call: Call,
        destination: String,
        slots: &mut HashMap<u32, SlotState>,
    ) {
        if crate::sip::outbound::validate_destination(&destination).is_err() {
            tracing::warn!(destination = %destination, "outbound: invalid destination, refusing");
            self.sip_bridge.refuse_outbound(call, 484);
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_invalid_destination"])
                .inc();
            return;
        }

        let Some((slot, audio_device, cmd_tx)) = claim_idle_cs_slot(slots) else {
            tracing::warn!(destination = %destination, "outbound: no idle line, refusing");
            self.sip_bridge.refuse_outbound(call, 503);
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_no_idle_line"])
                .inc();
            return;
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        if cmd_tx
            .send(ModuleCmd::Dial(destination.clone(), resp_tx))
            .is_err()
        {
            tracing::warn!(
                slot,
                destination = %destination,
                "outbound: module command channel closed, refusing"
            );
            release_slot(slots, slot);
            self.sip_bridge.refuse_outbound(call, 503);
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&["refused_no_idle_line"])
                .inc();
            return;
        }

        match tokio::time::timeout(Duration::from_secs(5), resp_rx).await {
            Ok(Ok(Ok(()))) => match self.sip_bridge.accept_outbound(call, &audio_device) {
                Ok(()) => {
                    tracing::info!(slot, destination = %destination, "outbound call placed");
                    metrics::OUTBOUND_ATTEMPTS_TOTAL
                        .with_label_values(&["placed"])
                        .inc();
                }
                Err(e) => {
                    tracing::error!(slot, error = %e, "outbound: failed to accept SIP leg after dial succeeded");
                    // ATD already succeeded — the modem is on a real,
                    // live call. Hang it up before freeing the slot, or
                    // it stays connected indefinitely while `SlotState`
                    // reports it idle and eligible for a second dial
                    // (specs/025-outbound-calling review).
                    let _ = cmd_tx.send(ModuleCmd::Hangup);
                    release_slot(slots, slot);
                    metrics::OUTBOUND_ATTEMPTS_TOTAL
                        .with_label_values(&["refused_network_failure"])
                        .inc();
                }
            },
            Ok(Ok(Err(e))) => {
                tracing::warn!(slot, error = %e, "outbound: dial failed");
                release_slot(slots, slot);
                self.sip_bridge.refuse_outbound(call, 503);
                metrics::OUTBOUND_ATTEMPTS_TOTAL
                    .with_label_values(&["refused_network_failure"])
                    .inc();
            }
            Ok(Err(_)) | Err(_) => {
                tracing::warn!(slot, "outbound: module did not respond in time");
                // The 5s timeout only bounds how long *we* wait — it says
                // nothing about the modem. If `apply_dial_cmd` is still
                // in flight and later succeeds, the call would otherwise
                // be a real, connected phone call nothing in this process
                // ever tracks or hangs up (same class of leak the
                // accept_outbound failure just above already guards
                // against). Best-effort: the worker processes one
                // `ModuleCmd` at a time, so this queues behind whatever
                // dial is still running and hangs it up the moment it
                // finishes either way; harmless if the worker already
                // died (`cmd_tx.send` just fails silently) or if the dial
                // genuinely never reached the modem.
                let _ = cmd_tx.send(ModuleCmd::Hangup);
                release_slot(slots, slot);
                self.sip_bridge.refuse_outbound(call, 503);
                metrics::OUTBOUND_ATTEMPTS_TOTAL
                    .with_label_values(&["refused_network_failure"])
                    .inc();
            }
        }
    }

    pub(super) async fn handle_control_cmd(
        &mut self,
        cmd: ControlCmd,
        reply: oneshot::Sender<ControlResp>,
        slots: &mut HashMap<u32, SlotState>,
        tasks: &mut JoinSet<(u32, String)>,
    ) {
        match cmd {
            ControlCmd::ListSlots => {
                let mut infos: Vec<SlotInfo> = slots.values().map(|s| s.info()).collect();
                infos.sort_by_key(|i| i.slot);
                let _ = reply.send(ControlResp::ok_slots(infos));
            }

            ControlCmd::GetMode { slot } => {
                if !slots.contains_key(&slot) {
                    let _ = reply.send(ControlResp::err(unknown_slot(slots, slot)));
                    return;
                }
                let mode = match self.store.get_mode_pref(slot) {
                    Ok(Some(m)) => m,
                    Ok(None) => NetworkMode::Auto,
                    Err(e) => {
                        let _ = reply.send(ControlResp::err(format!("DB error: {e}")));
                        return;
                    }
                };
                let _ = reply.send(ControlResp::ok_mode(mode));
            }

            ControlCmd::SetMode {
                slot,
                mode: mode_str,
            } => {
                let mode = match mode_str.parse::<NetworkMode>() {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = reply.send(ControlResp::err(e));
                        return;
                    }
                };

                let state = match slots.get(&slot) {
                    Some(s) => s,
                    None => {
                        let _ = reply.send(ControlResp::err(unknown_slot(slots, slot)));
                        return;
                    }
                };

                if state.lifecycle != LifecycleState::Ready {
                    let _ = reply.send(ControlResp::err(format!(
                        "slot {slot} is not in Ready state (current: {})",
                        state.lifecycle
                    )));
                    return;
                }

                let Some(cmd_tx) = state.cmd_tx.clone() else {
                    let _ = reply.send(ControlResp::err("module command channel not available"));
                    return;
                };

                let (resp_tx, resp_rx) = oneshot::channel();
                if cmd_tx.send(ModuleCmd::SetMode(mode, resp_tx)).is_err() {
                    let _ = reply.send(ControlResp::err("module command channel closed"));
                    return;
                }
                let store_tx = self.store.sender();
                // Await the response in a separate task to avoid holding &self across .await
                tokio::spawn(async move {
                    match tokio::time::timeout(Duration::from_secs(30), resp_rx).await {
                        Ok(Ok(Ok(confirmed))) => {
                            let _ = store_tx.send(StoreCommand::SetModePref {
                                slot,
                                mode: confirmed,
                            });
                            let _ = reply.send(ControlResp::ok_mode(confirmed));
                        }
                        Ok(Ok(Err(e))) => {
                            let _ = reply.send(ControlResp::err(format!("AT command failed: {e}")));
                        }
                        Ok(Err(_)) => {
                            let _ = reply.send(ControlResp::err("module did not respond"));
                        }
                        Err(_) => {
                            let _ = reply
                                .send(ControlResp::err("AT command timeout while applying mode"));
                        }
                    }
                });
            }

            ControlCmd::CardRestart { slot, mode } => {
                let mode = match mode.parse::<RestartMode>() {
                    Ok(m) => m,
                    Err(e) => {
                        let _ = reply.send(ControlResp::err(e));
                        return;
                    }
                };

                // FR-014a: cycle concurrency rules.
                use crate::modules::scheduler::{
                    handle_manual_restart_during_cycle, ManualRestartCycleAdvice,
                };
                let cycle_advice = self
                    .cycle
                    .as_mut()
                    .map(|c| handle_manual_restart_during_cycle(c, slot))
                    .unwrap_or(ManualRestartCycleAdvice::Proceed);
                match cycle_advice {
                    ManualRestartCycleAdvice::Reject { error } => {
                        let _ = reply.send(ControlResp::err(error));
                        return;
                    }
                    ManualRestartCycleAdvice::PreemptAndProceed => {
                        // The pure helper already pushed the outcome into the
                        // cycle's outcome log; mirror it to tracing+metrics.
                        let outcome = CycleOutcome {
                            slot,
                            attempt: AttemptType::Initial,
                            outcome: Outcome::AlreadyRestartedByManual,
                            duration: Duration::ZERO,
                        };
                        self.record_outcome(slot, &outcome);
                    }
                    ManualRestartCycleAdvice::Proceed => {}
                }

                let Some(state) = slots.get_mut(&slot) else {
                    let _ = reply.send(ControlResp::err(unknown_slot(slots, slot)));
                    return;
                };

                tracing::info!(
                    slot = slot,
                    module = %state.module.id,
                    mode = ?mode,
                    "card restart requested"
                );
                let retry_delay = retry_delay_for(mode, state.cmd_tx.is_some());
                if let Some(cmd_tx) = state.cmd_tx.take() {
                    // Worker is running — ask it to run the restart and exit.
                    let _ = cmd_tx.send(ModuleCmd::Reboot(mode));
                } else {
                    // Worker not running — same reasoning as
                    // apply_send_reboot's identical fallback (Greptile
                    // review, PR #30): run it on tokio's blocking pool,
                    // tracked in the same JoinSet every module worker
                    // lives in, rather than inline on this async
                    // handler's own event-loop thread.
                    tracing::info!(module = %state.module.id, "no worker running, rebooting modem directly");
                    super::restart_cycle::spawn_fallback_restart(
                        tasks,
                        state.module.serial_port.clone(),
                        state.module.id.clone(),
                        slot,
                        mode,
                        "card restart",
                    );
                }
                state.lifecycle = LifecycleState::Recovering;
                state.retry_count = 0;
                state.next_retry_at = Some(tokio::time::Instant::now() + retry_delay);
                let _ = reply.send(ControlResp::ok());
            }

            ControlCmd::Observe { .. } => {
                // `control::server::handle_connection` applies this to the
                // metrics registry directly and never forwards it here — an
                // Observe reaching CardPool would mean that short-circuit
                // regressed (specs/014-vowifi-metrics-restore).
                let _ = reply.send(ControlResp::err(
                    "observe commands are handled by the control server, not CardPool",
                ));
            }
        }
    }

    pub(super) fn handle_bridge_event(
        &mut self,
        event: BridgeEvent,
        slots: &mut HashMap<u32, SlotState>,
    ) {
        match event {
            BridgeEvent::NetworkLost { module_id } => {
                if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
                    if state.lifecycle == LifecycleState::Ready {
                        tracing::warn!(module = %module_id, slot = state.slot, "network lost, transitioning to Recovering");
                        state.lifecycle = LifecycleState::Recovering;
                        state.network_type = NetworkType::NoSignal;
                        state.cmd_tx = None;
                        state.retry_count = 0;
                        state.next_retry_at = Some(
                            tokio::time::Instant::now()
                                + backoff_delay(
                                    0,
                                    self.config.resilience.initial_backoff_sec,
                                    self.config.resilience.max_backoff_sec,
                                ),
                        );
                    }
                    // Network loss tears down any in-progress call; clear the flag so
                    // the scheduler does not permanently defer this slot.
                    if state.has_active_call {
                        tracing::warn!(module = %module_id, slot = state.slot, "active call terminated by network loss");
                        state.has_active_call = false;
                    }
                }
            }
            BridgeEvent::Ring {
                module_id,
                caller_id,
                audio_device,
            } => {
                self.handle_ring_event(slots, module_id, caller_id, audio_device);
            }
            BridgeEvent::Hangup { module_id } => {
                if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
                    state.has_active_call = false;
                }
                tracing::info!(module = %module_id, "GSM call ended, tearing down SIP call");
                self.sip_bridge.hangup_active_call();
            }
            BridgeEvent::SmsReceived {
                module_id,
                sender,
                body,
                received_at,
            } => {
                // specs/034-alert-identity: tag the forward with this card's
                // own number (from its slot), so the Discord message shows
                // which line received the SMS. One module_id can have several
                // slots (a stale GivenUp one beside a fresh Ready one, whose
                // `phone_number` is still empty) — pick the first slot that
                // actually has a number rather than depending on HashMap
                // iteration order.
                let phone_number = slots
                    .values()
                    .filter(|s| s.module.id == module_id)
                    .find_map(|s| s.alert_phone());
                sms::record_and_forward(
                    &tokio::runtime::Handle::current(),
                    self.store.sender(),
                    self.discord_client.clone(),
                    module_id,
                    sender,
                    body,
                    received_at,
                    crate::store::Transport::Cs,
                    None,
                    phone_number,
                );
            }
            BridgeEvent::CriticalAlert(event) => {
                if let Some(client) = self.alerts_client.clone() {
                    let config = self.config.alerts.clone();
                    tokio::runtime::Handle::current()
                        .spawn(async move { alerts::dispatch(&client, &config, event).await });
                }
            }
        }
    }

    /// The worker (`handle_ring`) already sent `ATA` and answered the call on
    /// real hardware *before* this event ever arrives — every early return
    /// below is therefore a real, live, connected GSM call with no SIP bridge
    /// for it, not just an aborted attempt. Found in review (Greptile, PR
    /// #22): a plain "clear the flag and give up" here left that call
    /// connected to dead air indefinitely, while `has_active_call = false`
    /// made the slot look idle and eligible for a *second* dial while the
    /// modem was still genuinely busy with the first.
    /// [`Self::hang_up_unbridged_call`] sends the same `ModuleCmd::Hangup`
    /// the outbound-dial path already uses for the equivalent "answered but
    /// couldn't bridge" case — it hangs up the real call, sets `card.state =
    /// Idle`, and closes out `call_ctx` via `record_call_end("failed")` —
    /// genuinely freeing the slot, not just pretending to.
    fn handle_ring_event(
        &mut self,
        slots: &mut HashMap<u32, SlotState>,
        module_id: String,
        caller_id: String,
        audio_device: String,
    ) {
        if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
            state.has_active_call = true;
        }

        if self.sip_bridge.state != crate::sip::RegistrationState::Registered {
            tracing::warn!(
                module = %module_id,
                "SIP not registered, cannot bridge call"
            );
            hang_up_unbridged_call(slots, &module_id);
            return;
        }

        // In SIP server mode this fails when the phone is not registered.
        let dest_uri = match self.sip_bridge.compute_destination_uri(&caller_id) {
            Ok(uri) => uri,
            Err(e) => {
                tracing::warn!(
                    module = %module_id,
                    caller = %caller_id,
                    error = %e,
                    "cannot bridge call: no phone to ring"
                );
                metrics::SIP_SERVER_RING_TARGET_MISSING_TOTAL.inc();
                metrics::SIP_CALLS_TOTAL
                    .with_label_values(&[&module_id, "error", "cs"])
                    .inc();
                hang_up_unbridged_call(slots, &module_id);
                return;
            }
        };
        tracing::info!(
            module = %module_id,
            caller = %caller_id,
            dest = %dest_uri,
            audio = %audio_device,
            "bridging GSM call to SIP"
        );

        if let Err(e) = self.sip_bridge.set_sound_device(&audio_device) {
            tracing::error!(error = %e, "failed to set sound device");
            metrics::AUDIO_ERRORS_TOTAL
                .with_label_values(&[&module_id, "sound_device"])
                .inc();
            hang_up_unbridged_call(slots, &module_id);
            return;
        }

        if let Err(e) = self.sip_bridge.make_call(&dest_uri, &caller_id) {
            tracing::error!(
                module = %module_id,
                error = %e,
                "SIP outbound call failed"
            );
            metrics::SIP_CALLS_TOTAL
                .with_label_values(&[&module_id, "error", "cs"])
                .inc();
            hang_up_unbridged_call(slots, &module_id);
        } else {
            metrics::SIP_CALLS_TOTAL
                .with_label_values(&[&module_id, "initiated", "cs"])
                .inc();
        }
    }
}

/// Frees a slot claimed by `claim_idle_cs_slot` when the dial did not end up
/// bridged. Was five identical inline copies in `handle_outbound_request`.
fn release_slot(slots: &mut HashMap<u32, SlotState>, slot: u32) {
    if let Some(state) = slots.get_mut(&slot) {
        state.has_active_call = false;
    }
}

/// Frees a slot whose GSM call was answered but could not be bridged, and
/// hangs up the real call behind it. See [`CardPool::handle_ring_event`].
fn hang_up_unbridged_call(slots: &mut HashMap<u32, SlotState>, module_id: &str) {
    if let Some(state) = slots.values_mut().find(|s| s.module.id == module_id) {
        state.has_active_call = false;
        if let Some(cmd_tx) = &state.cmd_tx {
            let _ = cmd_tx.send(ModuleCmd::Hangup);
        }
    }
}

/// The "slot N not found" error every control command returns for an
/// out-of-range slot, verbatim as before — three call sites had their own copy.
fn unknown_slot(slots: &HashMap<u32, SlotState>, slot: u32) -> String {
    let max = slots.keys().max().copied().unwrap_or(0);
    format!("slot {slot} not found; valid slots: 0..={max}")
}
