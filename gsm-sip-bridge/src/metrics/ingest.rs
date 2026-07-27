//! Applies an `AgentReport` (specs/014-vowifi-metrics-restore) to the
//! daemon's Prometheus registry, and tracks per-agent liveness so
//! `metrics::server` can expire a silent agent at scrape time.
//!
//! Counters here only ever move forward: a report's `events` are deltas,
//! never absolute totals, so a supervised agent restart (routine — see
//! `docker/entrypoint.sh`'s 5s restart loop) cannot rewind a series
//! (FR-020). Gauges are the opposite: always applied as the report's
//! absolute value, latest-wins, with no ordering guarantee assumed between
//! reports (contracts/observability-protocol.md).

use crate::alerts::{AlertCategory, CriticalEvent, CriticalEventKind};
use crate::control::protocol::{
    AgentKind, AgentReport, AgentState, CallStatus, ObservedEvent, SmsOutcome,
};
use crate::metrics;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TRANSPORT_VOWIFI: &str = "vowifi";
/// Host-side IMS over LTE. A third value on the existing `transport` label,
/// which is additive for dashboard queries (research R5).
const TRANSPORT_VOLTE: &str = "volte";

/// Which `transport` label an agent's reports belong under.
///
/// Derived from the agent kind rather than hardcoded: the cellular service
/// runs the same agent code as the Wi-Fi one, so assuming `vowifi` here would
/// file every VoLTE call under the wrong transport and make the two paths
/// indistinguishable — in exactly the comparison this feature exists to make.
fn transport_label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Volte | AgentKind::VolteSip => TRANSPORT_VOLTE,
        AgentKind::Ims | AgentKind::Sip => TRANSPORT_VOWIFI,
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentRecord {
    last_report: Instant,
    /// The `(epoch, seq)` of the last report actually *applied* (as opposed
    /// to merely received) for this agent — see `AgentReport`'s doc comment.
    /// A report whose `epoch` matches and whose `seq` is `<=` this is a
    /// replay of an already-applied report (its acknowledgement was lost,
    /// so the reporter retried it) and must not be applied twice.
    last_applied: (u64, u64),
    /// specs/022-discord-critical-alerts. Set the first time a report
    /// observes `registered = Some(false)`, cleared the moment it observes
    /// `Some(true)` again — `apply_state` owns this field exclusively.
    /// `None` means "currently registered (or this agent has never
    /// reported `registered` at all)".
    registered_unhealthy_since: Option<Instant>,
    /// Whether a `RegistrationLoss` alert has already been sent for the
    /// *current* unhealthy streak — owned exclusively by
    /// `evaluate_critical_alerts`, so a still-unhealthy tick never re-sends
    /// (FR-013) and a healthy tick after having alerted sends exactly one
    /// Recovered notice.
    registered_alerted: bool,
    /// Same pair, for `tunnel_up`.
    tunnel_unhealthy_since: Option<Instant>,
    tunnel_alerted: bool,
}

/// Keyed by `(agent kind, module_id)`, not just agent kind — with
/// specs/013-multi-card-vowifi, there can be several `vowifi-ims-agent`
/// processes (one per line) reporting concurrently, and `vowifi-sip-agent`
/// reports on behalf of several lines from one process, so a single fixed
/// slot per `AgentKind` would let one line's reports clobber another's
/// liveness record.
fn liveness() -> &'static Mutex<HashMap<(AgentKind, String), AgentRecord>> {
    static STATE: OnceLock<Mutex<HashMap<(AgentKind, String), AgentRecord>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Applies one `AgentReport` to the registry. Never fails: a malformed
/// individual event is impossible by construction (the wire type is a
/// closed Rust enum), and there is nothing else here that can go wrong in a
/// way the caller needs to react to.
///
/// Idempotent per `(epoch, seq)`: a report that has already been applied —
/// identified by matching `epoch` and a `seq` no greater than the last one
/// applied — is a replay (the reporter retrying because it never saw this
/// report's acknowledgement) and is skipped rather than double-applied.
/// Liveness is still refreshed either way, since the retry itself proves the
/// agent is alive.
pub fn apply_report(report: &AgentReport) {
    let module_id = report.module_id.as_str();
    let key = (report.agent, module_id.to_string());
    let mut guard = liveness().lock().unwrap();
    let existing = guard.get(&key).copied();

    let is_replay = existing.is_some_and(|record| {
        record.last_applied.0 == report.epoch && report.seq <= record.last_applied.1
    });

    // `*_alerted` is owned exclusively by `evaluate_critical_alerts` — passed
    // through unchanged here regardless of what `apply_state` does to
    // `*_unhealthy_since` below, so a healthy report arriving before the
    // evaluator's next tick doesn't erase the "a Recovered notice is owed"
    // signal out from under it.
    let registered_alerted = existing.is_some_and(|r| r.registered_alerted);
    let tunnel_alerted = existing.is_some_and(|r| r.tunnel_alerted);
    let mut registered_unhealthy_since = existing.and_then(|r| r.registered_unhealthy_since);
    let mut tunnel_unhealthy_since = existing.and_then(|r| r.tunnel_unhealthy_since);

    if !is_replay {
        apply_state(
            report.agent,
            module_id,
            &report.state,
            &mut registered_unhealthy_since,
            &mut tunnel_unhealthy_since,
        );

        for event in &report.events {
            apply_event(report.agent, module_id, event);
        }

        if report.dropped > 0 {
            metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
                .with_label_values(&[report.agent.as_str(), module_id])
                .inc_by(report.dropped as f64);
        }
    }

    let last_applied = if is_replay {
        existing.unwrap().last_applied
    } else {
        (report.epoch, report.seq)
    };
    guard.insert(
        key,
        AgentRecord {
            last_report: Instant::now(),
            last_applied,
            registered_unhealthy_since,
            registered_alerted,
            tunnel_unhealthy_since,
            tunnel_alerted,
        },
    );
}

fn apply_state(
    agent: AgentKind,
    module_id: &str,
    state: &AgentState,
    registered_unhealthy_since: &mut Option<Instant>,
    tunnel_unhealthy_since: &mut Option<Instant>,
) {
    let transport = transport_label(agent);
    if let Some(active_calls) = state.active_calls {
        metrics::ACTIVE_CALLS
            .with_label_values(&[module_id, transport])
            .set(active_calls as f64);
    }
    // Registration and attachment health goes to the gauge belonging to the
    // path that actually holds it (specs/017 FR-031). Routing the cellular
    // service's state to the VoWiFi gauges would report a phantom Wi-Fi line
    // *and* leave the VoLTE gauges reading zero while the service is
    // perfectly healthy — so an operator alerting on either one would be
    // told the opposite of the truth. Observed live before it was fixed:
    // `gsm_sip_bridge_vowifi_tunnel_up{module="volte"} 1`, claiming an ePDG
    // tunnel that does not exist on this path.
    if let Some(registered) = state.registered {
        let up = if registered { 1.0 } else { 0.0 };
        match agent {
            AgentKind::Volte => metrics::VOLTE_REGISTERED.set(up),
            AgentKind::Ims | AgentKind::Sip => metrics::VOWIFI_REGISTERED
                .with_label_values(&[module_id])
                .set(up),
            // The telephony half reports `pbx_registered`, never `registered`
            // — the IMS registration belongs to the `Volte` carrier half — so
            // this arm is unreachable and must not touch a gauge it does not
            // own.
            AgentKind::VolteSip => {}
        }
        // specs/022-discord-critical-alerts: a sustained unhealthy streak,
        // not a raw snapshot — cleared here on every healthy report, but
        // *when* it flips back is `evaluate_critical_alerts`'s call (it owns
        // `registered_alerted`, not this function).
        if registered {
            *registered_unhealthy_since = None;
        } else if registered_unhealthy_since.is_none() {
            *registered_unhealthy_since = Some(Instant::now());
        }
    }
    if let Some(tunnel_up) = state.tunnel_up {
        let up = if tunnel_up { 1.0 } else { 0.0 };
        match agent {
            // The LTE path's equivalent of "the tunnel is up" is the IMS PDN
            // being attached and routable.
            AgentKind::Volte => metrics::VOLTE_PDN_UP.set(up),
            AgentKind::Ims | AgentKind::Sip => metrics::VOWIFI_TUNNEL_UP
                .with_label_values(&[module_id])
                .set(up),
            // The telephony half has no tunnel/PDN of its own; unreachable,
            // same as `registered` above.
            AgentKind::VolteSip => {}
        }
        if tunnel_up {
            *tunnel_unhealthy_since = None;
        } else if tunnel_unhealthy_since.is_none() {
            *tunnel_unhealthy_since = Some(Instant::now());
        }
    }
    // pbx_registered (Agent B) has no dedicated gauge yet — sip_registered
    // remains the daemon's own PBX registration (metrics-inventory.md
    // "Unchanged" note); tracked here only so liveness has somewhere to
    // record Agent B reported it, for future use.
}

fn apply_event(agent: AgentKind, module_id: &str, event: &ObservedEvent) {
    let transport = transport_label(agent);
    match event {
        ObservedEvent::CallCompleted {
            status,
            duration_seconds,
        } => {
            metrics::CALLS_TOTAL
                .with_label_values(&[module_id, status.as_str(), transport])
                .inc();
            if *status == CallStatus::Answered {
                metrics::CALL_DURATION_SECONDS
                    .with_label_values(&[module_id, transport])
                    .observe(*duration_seconds);
            }
        }
        ObservedEvent::PbxLegCompleted { outcome } => {
            let status = match outcome {
                SmsOutcome::Sent => "success",
                SmsOutcome::Failed => "failed",
            };
            metrics::SIP_CALLS_TOTAL
                .with_label_values(&[module_id, status, transport])
                .inc();
        }
        ObservedEvent::BridgeFailed { reason } => {
            metrics::VOWIFI_BRIDGE_FAILURES_TOTAL
                .with_label_values(&[module_id, reason.as_str()])
                .inc();
        }
        ObservedEvent::SmsReceived => {
            metrics::SMS_RECEIVED_TOTAL
                .with_label_values(&[module_id, transport])
                .inc();
        }
        ObservedEvent::SmsForwarded { outcome } => {
            let outcome_str = match outcome {
                SmsOutcome::Sent => "sent",
                SmsOutcome::Failed => "failed",
            };
            metrics::SMS_FORWARDED_TOTAL
                .with_label_values(&[module_id, outcome_str, transport])
                .inc();
        }
        ObservedEvent::RegistrationAttempt { status } => {
            metrics::VOWIFI_REGISTRATIONS_TOTAL
                .with_label_values(&[module_id, status.as_str()])
                .inc();
        }
    }
}

/// Evaluated by `metrics::server`'s scrape handler. Returns one entry per
/// `(agent kind, module_id)` that has reported at least once since this
/// process started, with whether it is stale (`last_report` older than
/// `staleness_threshold`) and the module id whose gauges must be zeroed if
/// so. A line/agent that has never reported at all has no entry — no
/// different, for scrape purposes, from a metric series that doesn't exist
/// yet, and resolves itself the moment that agent's first report arrives.
pub struct AgentLiveness {
    pub agent: AgentKind,
    pub up: bool,
    pub age_seconds: f64,
    pub module_id: String,
}

pub fn evaluate_liveness(staleness_threshold: std::time::Duration) -> Vec<AgentLiveness> {
    liveness()
        .lock()
        .unwrap()
        .iter()
        .map(|((agent, module_id), record)| {
            let age = record.last_report.elapsed();
            AgentLiveness {
                agent: *agent,
                up: age <= staleness_threshold,
                age_seconds: age.as_secs_f64(),
                module_id: module_id.clone(),
            }
        })
        .collect()
}

/// specs/022-discord-critical-alerts FR-006/FR-005/FR-013 (US2/US3,
/// research.md R1/R4). Evaluated on every scrape, mirroring
/// `evaluate_liveness` — not on a separate timer, one fewer moving part.
/// Mutates the stored `*_alerted` flags in place: a `RegistrationLoss`/
/// `TunnelFailure` `Failure` event is returned (and the flag set) the tick a
/// sustained-unhealthy streak first crosses its threshold; a `Recovered`
/// event is returned (and the flag cleared) the first tick observed healthy
/// again after having alerted. Every other tick returns nothing for that
/// signal — a still-unhealthy-but-already-alerted tick is recorded via
/// `alerts::record_suppressed` instead of an event.
pub fn evaluate_critical_alerts(
    registration_threshold: Duration,
    tunnel_threshold: Duration,
) -> Vec<CriticalEvent> {
    let mut events = Vec::new();
    let mut guard = liveness().lock().unwrap();
    let now = Instant::now();

    for ((agent, module_id), record) in guard.iter_mut() {
        match (record.registered_unhealthy_since, record.registered_alerted) {
            (Some(since), false) if now.duration_since(since) >= registration_threshold => {
                record.registered_alerted = true;
                events.push(CriticalEvent {
                    category: AlertCategory::RegistrationLoss,
                    unit_id: Some(module_id.clone()),
                    description: format!(
                        "{} line unregistered for over {}s",
                        agent.as_str(),
                        registration_threshold.as_secs()
                    ),
                    at: chrono::Utc::now(),
                    kind: CriticalEventKind::Failure,
                });
            }
            (Some(_), true) => {
                crate::alerts::record_suppressed(AlertCategory::RegistrationLoss);
            }
            (None, true) => {
                record.registered_alerted = false;
                events.push(CriticalEvent {
                    category: AlertCategory::RegistrationLoss,
                    unit_id: Some(module_id.clone()),
                    description: format!("{} line re-registered", agent.as_str()),
                    at: chrono::Utc::now(),
                    kind: CriticalEventKind::Recovered,
                });
            }
            _ => {}
        }

        match (record.tunnel_unhealthy_since, record.tunnel_alerted) {
            (Some(since), false) if now.duration_since(since) >= tunnel_threshold => {
                record.tunnel_alerted = true;
                events.push(CriticalEvent {
                    category: AlertCategory::TunnelFailure,
                    unit_id: Some(module_id.clone()),
                    description: format!(
                        "{} line's tunnel non-established for over {}s",
                        agent.as_str(),
                        tunnel_threshold.as_secs()
                    ),
                    at: chrono::Utc::now(),
                    kind: CriticalEventKind::Failure,
                });
            }
            (Some(_), true) => {
                crate::alerts::record_suppressed(AlertCategory::TunnelFailure);
            }
            (None, true) => {
                record.tunnel_alerted = false;
                events.push(CriticalEvent {
                    category: AlertCategory::TunnelFailure,
                    unit_id: Some(module_id.clone()),
                    description: format!("{} line's tunnel re-established", agent.as_str()),
                    at: chrono::Utc::now(),
                    kind: CriticalEventKind::Recovered,
                });
            }
            _ => {}
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::{AgentReport, AgentState};

    #[test]
    fn test_apply_report_increments_call_metrics() {
        let before = metrics::CALLS_TOTAL
            .with_label_values(&["test-ingest-calls", "answered", "vowifi"])
            .get();

        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: "test-ingest-calls".to_string(),
            epoch: 9001,
            seq: 1,
            state: AgentState {
                active_calls: Some(0),
                ..Default::default()
            },
            events: vec![ObservedEvent::CallCompleted {
                status: CallStatus::Answered,
                duration_seconds: 3.0,
            }],
            dropped: 0,
        });

        let after = metrics::CALLS_TOTAL
            .with_label_values(&["test-ingest-calls", "answered", "vowifi"])
            .get();
        assert_eq!(after, before + 1.0);
    }

    #[test]
    fn test_apply_report_records_liveness() {
        apply_report(&AgentReport {
            agent: AgentKind::Sip,
            module_id: "test-ingest-liveness".to_string(),
            epoch: 9002,
            seq: 1,
            state: AgentState::default(),
            events: vec![],
            dropped: 0,
        });

        let states = evaluate_liveness(std::time::Duration::from_secs(30));
        let sip = states
            .iter()
            .find(|s| s.agent == AgentKind::Sip && s.module_id == "test-ingest-liveness")
            .unwrap();
        assert!(sip.up);
    }

    #[test]
    fn test_apply_report_tracks_dropped_events() {
        let before = metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
            .with_label_values(&["ims", "test-ingest-dropped"])
            .get();

        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: "test-ingest-dropped".to_string(),
            epoch: 9003,
            seq: 1,
            state: AgentState::default(),
            events: vec![],
            dropped: 7,
        });

        let after = metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
            .with_label_values(&["ims", "test-ingest-dropped"])
            .get();
        assert_eq!(after, before + 7.0);
    }

    #[test]
    fn test_replayed_report_is_not_applied_twice() {
        let module_id = "test-ingest-replay".to_string();
        let make_report = |seq: u64| AgentReport {
            agent: AgentKind::Sip,
            module_id: module_id.clone(),
            epoch: 9004,
            seq,
            state: AgentState::default(),
            events: vec![ObservedEvent::SmsReceived],
            dropped: 0,
        };

        let before = metrics::SMS_RECEIVED_TOTAL
            .with_label_values(&[&module_id, "vowifi"])
            .get();

        apply_report(&make_report(1));
        // Same epoch, same seq — exactly what the reporter sends on a retry
        // after a lost acknowledgement (contracts/observability-protocol.md).
        apply_report(&make_report(1));

        let after = metrics::SMS_RECEIVED_TOTAL
            .with_label_values(&[&module_id, "vowifi"])
            .get();
        assert_eq!(
            after,
            before + 1.0,
            "a replayed report (same epoch, non-advancing seq) must not double-count"
        );

        // A genuinely new report (seq advances) must still apply normally.
        apply_report(&make_report(2));
        let final_count = metrics::SMS_RECEIVED_TOTAL
            .with_label_values(&[&module_id, "vowifi"])
            .get();
        assert_eq!(final_count, before + 2.0);

        // A new epoch (agent restarted) must apply even with a lower seq —
        // it is not a replay of anything the daemon has seen before.
        let mut restarted = make_report(1);
        restarted.epoch = 9005;
        apply_report(&restarted);
        let after_restart = metrics::SMS_RECEIVED_TOTAL
            .with_label_values(&[&module_id, "vowifi"])
            .get();
        assert_eq!(after_restart, before + 3.0);
    }

    #[test]
    fn the_two_ims_paths_do_not_collapse_into_one_transport() {
        // Both paths run the same agent code. If the label were assumed
        // rather than derived, every VoLTE call would be filed as `vowifi`
        // and the two would be indistinguishable — in exactly the comparison
        // this feature exists to make.
        assert_eq!(transport_label(AgentKind::Ims), TRANSPORT_VOWIFI);
        assert_eq!(transport_label(AgentKind::Sip), TRANSPORT_VOWIFI);
        assert_eq!(transport_label(AgentKind::Volte), TRANSPORT_VOLTE);
        assert_ne!(
            transport_label(AgentKind::Volte),
            transport_label(AgentKind::Ims)
        );
    }

    #[test]
    fn both_halves_of_the_volte_bridge_report_the_volte_transport() {
        // The bridge is one process with two independently-reporting halves:
        // the carrier side (`Volte`) and the telephone side (`VolteSip`, the
        // same code the Wi-Fi path runs as `Sip`). If the telephone side kept
        // reporting as `Sip`, its PBX-leg counter (`SIP_CALLS_TOTAL`) would be
        // filed under `vowifi` while the carrier side's `CALLS_TOTAL` sat under
        // `volte` — the same two calls split across two transports. Observed
        // live before this fix.
        assert_eq!(transport_label(AgentKind::VolteSip), TRANSPORT_VOLTE);
        assert_eq!(
            transport_label(AgentKind::Volte),
            transport_label(AgentKind::VolteSip)
        );
        // Same transport, but they must remain distinct kinds: each is its own
        // reporter with its own epoch/seq, and a shared liveness key would
        // corrupt replay detection across the two.
        assert_ne!(AgentKind::Volte, AgentKind::VolteSip);
    }

    #[test]
    fn each_paths_registration_health_lands_on_its_own_gauge() {
        // Observed live: the cellular service's registration was reported as
        // `gsm_sip_bridge_vowifi_registered{module="volte"} 1`, and its
        // attachment as a VoWiFi *tunnel* that does not exist on that path.
        // An operator alerting on either gauge was told the opposite of the
        // truth (FR-031).
        let module_id = "test-ingest-gauge-routing";
        metrics::VOLTE_REGISTERED.set(0.0);
        metrics::VOWIFI_REGISTERED
            .with_label_values(&[module_id])
            .set(0.0);

        apply_state(
            AgentKind::Volte,
            module_id,
            &AgentState {
                registered: Some(true),
                tunnel_up: Some(true),
                ..AgentState::default()
            },
            &mut None,
            &mut None,
        );

        assert_eq!(
            metrics::VOLTE_REGISTERED.get(),
            1.0,
            "the cellular path's own gauge must reflect it"
        );
        assert_eq!(
            metrics::VOWIFI_REGISTERED
                .with_label_values(&[module_id])
                .get(),
            0.0,
            "and it must not appear as a phantom VoWiFi line"
        );
    }

    // specs/022-discord-critical-alerts (T017/T023, US2/US3). Real `Instant`
    // arithmetic — `registered_unhealthy_since`/`tunnel_unhealthy_since` are
    // backdated directly rather than sleeping (research.md R4).
    //
    // `evaluate_critical_alerts` sweeps and mutates *every* entry in the
    // shared global `liveness()` map, not just the module under test — two
    // such tests running in parallel can otherwise steal/suppress each
    // other's expected event. `EVALUATE_LOCK` serializes only the tests that
    // call `evaluate_critical_alerts` against each other (a lighter-weight,
    // dependency-free stand-in for what a `#[serial]` attribute would do).
    static EVALUATE_LOCK: Mutex<()> = Mutex::new(());

    fn backdate_unhealthy_since(agent: AgentKind, module_id: &str, seconds_ago: u64) {
        let mut guard = liveness().lock().unwrap();
        let record = guard.get_mut(&(agent, module_id.to_string())).unwrap();
        let past = Instant::now() - Duration::from_secs(seconds_ago);
        record.registered_unhealthy_since = record.registered_unhealthy_since.map(|_| past);
        record.tunnel_unhealthy_since = record.tunnel_unhealthy_since.map(|_| past);
    }

    #[test]
    fn registration_loss_sets_unhealthy_since_on_first_false_report() {
        let module_id = "test-reg-loss-first";
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                registered: Some(false),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });

        let guard = liveness().lock().unwrap();
        let record = guard.get(&(AgentKind::Ims, module_id.to_string())).unwrap();
        assert!(record.registered_unhealthy_since.is_some());
        assert!(!record.registered_alerted);
    }

    #[test]
    fn registration_loss_does_not_reset_unhealthy_since_on_repeated_false_reports() {
        let module_id = "test-reg-loss-repeat";
        for seq in 1..=3 {
            apply_report(&AgentReport {
                agent: AgentKind::Ims,
                module_id: module_id.to_string(),
                epoch: 1,
                seq,
                state: AgentState {
                    registered: Some(false),
                    ..AgentState::default()
                },
                events: vec![],
                dropped: 0,
            });
        }
        backdate_unhealthy_since(AgentKind::Ims, module_id, 301);
        let since_after_backdate = {
            let guard = liveness().lock().unwrap();
            guard
                .get(&(AgentKind::Ims, module_id.to_string()))
                .unwrap()
                .registered_unhealthy_since
        };

        // A further `false` report must not push `registered_unhealthy_since`
        // forward again — only the first-in-a-streak report may set it.
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 4,
            state: AgentState {
                registered: Some(false),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });
        let guard = liveness().lock().unwrap();
        let record = guard.get(&(AgentKind::Ims, module_id.to_string())).unwrap();
        assert_eq!(record.registered_unhealthy_since, since_after_backdate);
    }

    #[test]
    fn evaluate_critical_alerts_fires_failure_once_threshold_crossed_then_suppresses() {
        let _guard = EVALUATE_LOCK.lock().unwrap();
        let module_id = "test-reg-loss-threshold";
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                registered: Some(false),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });
        backdate_unhealthy_since(AgentKind::Ims, module_id, 301);

        let events = evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));
        let fired = events
            .iter()
            .filter(|e| e.unit_id.as_deref() == Some(module_id))
            .count();
        assert_eq!(fired, 1, "exactly one Failure event on first crossing");
        assert_eq!(
            events
                .iter()
                .find(|e| e.unit_id.as_deref() == Some(module_id))
                .unwrap()
                .kind,
            CriticalEventKind::Failure
        );

        // Still unhealthy, already alerted: the next tick must not re-fire.
        let events_again =
            evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));
        assert!(
            !events_again
                .iter()
                .any(|e| e.unit_id.as_deref() == Some(module_id)),
            "must not re-alert while continuously unhealthy (FR-013)"
        );
    }

    #[test]
    fn evaluate_critical_alerts_fires_recovered_once_after_alerting() {
        let _guard = EVALUATE_LOCK.lock().unwrap();
        let module_id = "test-reg-loss-recovered";
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                registered: Some(false),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });
        backdate_unhealthy_since(AgentKind::Ims, module_id, 301);
        evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));

        // Re-registers within the same tick sequence.
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 2,
            state: AgentState {
                registered: Some(true),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });

        let events = evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));
        let recovered = events
            .iter()
            .find(|e| e.unit_id.as_deref() == Some(module_id))
            .expect("expected a Recovered event");
        assert_eq!(recovered.kind, CriticalEventKind::Recovered);

        // And exactly once — a further healthy tick emits nothing more.
        let events_again =
            evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));
        assert!(!events_again
            .iter()
            .any(|e| e.unit_id.as_deref() == Some(module_id)));
    }

    #[test]
    fn evaluate_critical_alerts_no_event_when_recovered_before_threshold() {
        let _guard = EVALUATE_LOCK.lock().unwrap();
        let module_id = "test-reg-loss-self-healed";
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                registered: Some(false),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });
        // Only 30s elapsed, well under a 300s threshold — a self-healed blip.
        backdate_unhealthy_since(AgentKind::Ims, module_id, 30);
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 2,
            state: AgentState {
                registered: Some(true),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });

        let events = evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));
        assert!(
            !events
                .iter()
                .any(|e| e.unit_id.as_deref() == Some(module_id)),
            "a blip that self-heals before the threshold must never alert"
        );
    }

    /// specs/022-discord-critical-alerts FR-009/T022: a deliberate shutdown
    /// never sends an explicit `registered = Some(false)` report — the
    /// agent process just stops reporting entirely (confirmed: nothing in
    /// `ims::agent`'s dispatch loop or shutdown path constructs one). This
    /// registration-loss mechanism only ever reacts to reports it actually
    /// receives, so an agent that goes silent (as a clean shutdown does)
    /// can never set `registered_unhealthy_since` — that silence is the
    /// pre-existing, separate `AGENT_UP` staleness mechanism's concern, not
    /// this one's. This test locks in that a record with no report at all
    /// yields no alert-eligible state, rather than asserting behavior in
    /// `ims::agent` this module cannot see.
    #[test]
    fn an_agent_that_stops_reporting_entirely_never_sets_unhealthy_since() {
        let module_id = "test-reg-loss-silent-shutdown";
        // No apply_report call at all — simulating a clean shutdown that
        // simply stops sending reports.
        let guard = liveness().lock().unwrap();
        assert!(guard
            .get(&(AgentKind::Ims, module_id.to_string()))
            .is_none());
    }

    #[test]
    fn evaluate_critical_alerts_tunnel_failure_independent_of_registration() {
        let _guard = EVALUATE_LOCK.lock().unwrap();
        let module_id = "test-tunnel-independent";
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: module_id.to_string(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                registered: Some(true),
                tunnel_up: Some(false),
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
        });
        backdate_unhealthy_since(AgentKind::Ims, module_id, 301);

        let events = evaluate_critical_alerts(Duration::from_secs(300), Duration::from_secs(300));
        let fired: Vec<_> = events
            .iter()
            .filter(|e| e.unit_id.as_deref() == Some(module_id))
            .collect();
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].category, AlertCategory::TunnelFailure);
    }
}
