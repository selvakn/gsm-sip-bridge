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

use crate::control::protocol::{
    AgentKind, AgentReport, AgentState, CallStatus, ObservedEvent, SmsOutcome,
};
use crate::metrics;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const TRANSPORT_VOWIFI: &str = "vowifi";

#[derive(Debug, Clone, Copy)]
struct AgentRecord {
    last_report: Instant,
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
pub fn apply_report(report: &AgentReport) {
    let module_id = report.module_id.as_str();

    apply_state(report.agent, module_id, &report.state);

    for event in &report.events {
        apply_event(module_id, event);
    }

    if report.dropped > 0 {
        metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
            .with_label_values(&[report.agent.as_str(), module_id])
            .inc_by(report.dropped as f64);
    }

    liveness().lock().unwrap().insert(
        (report.agent, module_id.to_string()),
        AgentRecord {
            last_report: Instant::now(),
        },
    );
}

fn apply_state(agent: AgentKind, module_id: &str, state: &AgentState) {
    if let Some(active_calls) = state.active_calls {
        metrics::ACTIVE_CALLS
            .with_label_values(&[module_id, TRANSPORT_VOWIFI])
            .set(active_calls as f64);
    }
    if let Some(registered) = state.registered {
        metrics::VOWIFI_REGISTERED
            .with_label_values(&[module_id])
            .set(if registered { 1.0 } else { 0.0 });
    }
    if let Some(tunnel_up) = state.tunnel_up {
        metrics::VOWIFI_TUNNEL_UP
            .with_label_values(&[module_id])
            .set(if tunnel_up { 1.0 } else { 0.0 });
    }
    // pbx_registered (Agent B) has no dedicated gauge yet — sip_registered
    // remains the daemon's own PBX registration (metrics-inventory.md
    // "Unchanged" note); tracked here only so liveness has somewhere to
    // record Agent B reported it, for future use.
    let _ = agent;
}

fn apply_event(module_id: &str, event: &ObservedEvent) {
    match event {
        ObservedEvent::CallCompleted {
            status,
            duration_seconds,
        } => {
            metrics::CALLS_TOTAL
                .with_label_values(&[module_id, status.as_str(), TRANSPORT_VOWIFI])
                .inc();
            if *status == CallStatus::Answered {
                metrics::CALL_DURATION_SECONDS
                    .with_label_values(&[module_id, TRANSPORT_VOWIFI])
                    .observe(*duration_seconds);
            }
        }
        ObservedEvent::PbxLegCompleted { outcome } => {
            let status = match outcome {
                SmsOutcome::Sent => "success",
                SmsOutcome::Failed => "failed",
            };
            metrics::SIP_CALLS_TOTAL
                .with_label_values(&[module_id, status, TRANSPORT_VOWIFI])
                .inc();
        }
        ObservedEvent::BridgeFailed { reason } => {
            metrics::VOWIFI_BRIDGE_FAILURES_TOTAL
                .with_label_values(&[module_id, reason.as_str()])
                .inc();
        }
        ObservedEvent::SmsReceived => {
            metrics::SMS_RECEIVED_TOTAL
                .with_label_values(&[module_id, TRANSPORT_VOWIFI])
                .inc();
        }
        ObservedEvent::SmsForwarded { outcome } => {
            let outcome_str = match outcome {
                SmsOutcome::Sent => "sent",
                SmsOutcome::Failed => "failed",
            };
            metrics::SMS_FORWARDED_TOTAL
                .with_label_values(&[module_id, outcome_str, TRANSPORT_VOWIFI])
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
            state: AgentState::default(),
            events: vec![],
            dropped: 0,
        });

        let states = evaluate_liveness(std::time::Duration::from_secs(30));
        let sip = states.iter().find(|s| s.agent == AgentKind::Sip).unwrap();
        assert!(sip.up);
        assert_eq!(sip.module_id, "test-ingest-liveness");
    }

    #[test]
    fn test_apply_report_tracks_dropped_events() {
        let before = metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
            .with_label_values(&["ims", "test-ingest-dropped"])
            .get();

        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: "test-ingest-dropped".to_string(),
            state: AgentState::default(),
            events: vec![],
            dropped: 7,
        });

        let after = metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
            .with_label_values(&["ims", "test-ingest-dropped"])
            .get();
        assert_eq!(after, before + 7.0);
    }
}
