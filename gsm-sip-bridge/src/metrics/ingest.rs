//! Applies an `AgentReport` (specs/014-vowifi-metrics-restore) to the
//! daemon's Prometheus registry, and tracks per-agent liveness so
//! `metrics::server` can expire a silent agent at scrape time.
//!
//! Counters here only ever move forward: a report's `events` are deltas,
//! never absolute totals, so a supervised agent restart (routine — see
//! `supervise::orchestrate`'s 5s restart loop) cannot rewind a series
//! (FR-020). Gauges are the opposite: always applied as the report's
//! absolute value, latest-wins, with no ordering guarantee assumed between
//! reports (contracts/observability-protocol.md).

use crate::alerts::discord::DiscordClient;
use crate::alerts::{AlertCategory, AlertOutcome, CriticalEvent, CriticalEventKind};
use crate::config::AlertsConfig;
use crate::control::protocol::{
    AgentKind, AgentReport, AgentState, CallStatus, ObservedEvent, SmsOutcome,
};
use crate::metrics;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// specs/022-discord-critical-alerts (Greptile P1 fix). Set once from
/// `main.rs` via [`init_alerts`], before the control server starts
/// accepting connections — so alert evaluation happens the moment each
/// real `AgentReport` arrives (report cadence, `[metrics]
/// .agent_report_interval_seconds`), not gated on an external Prometheus
/// scrape. A line with no scraper configured, or one that flaps between
/// two scrapes, is now evaluated exactly like one that gets scraped every
/// second — the two used to behave differently, which is the bug.
static ALERTS_CONFIG: OnceLock<AlertsConfig> = OnceLock::new();
static ALERTS_CLIENT: OnceLock<DiscordClient> = OnceLock::new();
/// specs/034-alert-identity: `unit_id → phone number` for the categories
/// evaluated here, which only know the reporting line's `unit_id`
/// (`crate::alerts::line_phone_map`). Empty when no line configures an msisdn.
static ALERTS_PHONE_MAP: OnceLock<std::collections::HashMap<String, String>> = OnceLock::new();

/// Called once from `main.rs`. A missing call (e.g. `DiscordClient::new`
/// failing, which in practice only happens if the HTTP client itself
/// can't be built) leaves alert dispatch permanently skipped — reports are
/// still ingested and gauges still update normally either way.
pub fn init_alerts(
    config: AlertsConfig,
    client: DiscordClient,
    phone_map: std::collections::HashMap<String, String>,
) {
    ALERTS_CONFIG.get_or_init(|| config);
    ALERTS_CLIENT.get_or_init(|| client);
    ALERTS_PHONE_MAP.get_or_init(|| phone_map);
}

/// A category's alert lifecycle for one signal (registration or tunnel),
/// independent of the raw `*_unhealthy_since` streak tracked alongside it.
/// Splitting "is this condition currently unhealthy" from "have we told
/// anyone about it yet" is what lets a failed Discord delivery be retried
/// instead of silently and permanently swallowed (Greptile P1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlertPhase {
    /// Healthy, or unhealthy but not yet past threshold, or unhealthy past
    /// threshold with the Failure delivery not yet dispatched (or dispatched
    /// and confirmed failed — see `record_alert_outcome`).
    Idle,
    /// Threshold crossed; a Failure dispatch is in flight. Held here (not
    /// jumped straight to `Alerted`) so a still-unhealthy report arriving
    /// before the dispatch resolves doesn't fire a second, overlapping send.
    Pending,
    /// Failure dispatch confirmed delivered (2xx from Discord). Stays here
    /// until a healthy report fires the `Recovered` notice.
    Alerted,
}

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
    registered_alert_phase: AlertPhase,
    /// Same pair, for `tunnel_up`.
    tunnel_unhealthy_since: Option<Instant>,
    tunnel_alert_phase: AlertPhase,
    /// Same pair, for `gm_connection_up` (specs/028-gm-tcp-reconnect).
    gm_connection_unhealthy_since: Option<Instant>,
    gm_connection_alert_phase: AlertPhase,
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

    // Both the state update and the alert-transition decision happen under
    // one lock acquisition so two reports for the same key can never race
    // into deciding the same threshold-crossing twice (`AlertPhase::Pending`
    // is set atomically with the rest of the record).
    let mut guard = liveness().lock().unwrap();
    let existing = guard.get(&key).copied();

    let is_replay = existing.is_some_and(|record| {
        record.last_applied.0 == report.epoch && report.seq <= record.last_applied.1
    });

    let mut registered_alert_phase =
        existing.map_or(AlertPhase::Idle, |r| r.registered_alert_phase);
    let mut tunnel_alert_phase = existing.map_or(AlertPhase::Idle, |r| r.tunnel_alert_phase);
    let mut gm_connection_alert_phase =
        existing.map_or(AlertPhase::Idle, |r| r.gm_connection_alert_phase);
    let mut registered_unhealthy_since = existing.and_then(|r| r.registered_unhealthy_since);
    let mut tunnel_unhealthy_since = existing.and_then(|r| r.tunnel_unhealthy_since);
    let mut gm_connection_unhealthy_since = existing.and_then(|r| r.gm_connection_unhealthy_since);
    // (category, transition, generation) to dispatch once the lock is
    // released. `generation` is the `*_unhealthy_since` value this
    // particular transition was decided against — threaded through to
    // `record_alert_outcome` so a delivery callback that resolves after a
    // *later* incident has already started can recognize it's stale and
    // become a no-op instead of corrupting the new incident's phase
    // (Greptile P1 follow-up: "delivery callbacks not scoped to an incident
    // generation").
    let mut pending_transitions: Vec<(AlertCategory, CriticalEventKind, Option<Instant>)> =
        Vec::new();

    if !is_replay {
        apply_state(
            report.agent,
            module_id,
            &report.state,
            &mut registered_unhealthy_since,
            &mut tunnel_unhealthy_since,
            &mut gm_connection_unhealthy_since,
        );

        for event in &report.events {
            apply_event(report.agent, module_id, event);
        }

        if report.dropped > 0 {
            metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
                .with_label_values(&[report.agent.as_str(), module_id])
                .inc_by(report.dropped as f64);
        }

        if let Some(cfg) = ALERTS_CONFIG.get() {
            let registration_threshold =
                Duration::from_secs(cfg.registration_loss_thresholds.unhealthy_sec);
            let tunnel_threshold = Duration::from_secs(cfg.tunnel_failure_thresholds.unhealthy_sec);

            match decide_transition(
                registered_unhealthy_since,
                &mut registered_alert_phase,
                registration_threshold,
            ) {
                Transition::Event(kind) => pending_transitions.push((
                    AlertCategory::RegistrationLoss,
                    kind,
                    registered_unhealthy_since,
                )),
                Transition::Suppressed => {
                    crate::alerts::record_suppressed(AlertCategory::RegistrationLoss)
                }
                Transition::None => {}
            }
            match decide_transition(
                tunnel_unhealthy_since,
                &mut tunnel_alert_phase,
                tunnel_threshold,
            ) {
                Transition::Event(kind) => pending_transitions.push((
                    AlertCategory::TunnelFailure,
                    kind,
                    tunnel_unhealthy_since,
                )),
                Transition::Suppressed => {
                    crate::alerts::record_suppressed(AlertCategory::TunnelFailure)
                }
                Transition::None => {}
            }
            let gm_connection_threshold =
                Duration::from_secs(cfg.gm_connection_lost_thresholds.unhealthy_sec);
            match decide_transition(
                gm_connection_unhealthy_since,
                &mut gm_connection_alert_phase,
                gm_connection_threshold,
            ) {
                Transition::Event(kind) => pending_transitions.push((
                    AlertCategory::GmConnectionLost,
                    kind,
                    gm_connection_unhealthy_since,
                )),
                Transition::Suppressed => {
                    crate::alerts::record_suppressed(AlertCategory::GmConnectionLost)
                }
                Transition::None => {}
            }
        }
    }

    let last_applied = if is_replay {
        existing.unwrap().last_applied
    } else {
        (report.epoch, report.seq)
    };
    guard.insert(
        key.clone(),
        AgentRecord {
            last_report: Instant::now(),
            last_applied,
            registered_unhealthy_since,
            registered_alert_phase,
            tunnel_unhealthy_since,
            tunnel_alert_phase,
            gm_connection_unhealthy_since,
            gm_connection_alert_phase,
        },
    );
    drop(guard);

    for (category, kind, generation) in pending_transitions {
        dispatch_transition(key.clone(), category, kind, generation);
    }
}

/// The pure per-signal decision: given the current unhealthy streak and
/// alert phase, what (if anything) should happen this report. Kept free of
/// I/O so it's directly unit-testable with backdated `Instant`s
/// (research.md R4) — no sleeping, no mock.
fn decide_transition(
    unhealthy_since: Option<Instant>,
    phase: &mut AlertPhase,
    threshold: Duration,
) -> Transition {
    match (unhealthy_since, *phase) {
        (Some(since), AlertPhase::Idle) if since.elapsed() >= threshold => {
            *phase = AlertPhase::Pending;
            Transition::Event(CriticalEventKind::Failure)
        }
        // Already dispatching a Failure for this streak — don't fire a
        // second, overlapping send (Greptile P1: this is also exactly what
        // stops a burst of reports from spawning duplicate deliveries).
        (Some(_), AlertPhase::Pending) => Transition::None,
        (Some(_), AlertPhase::Alerted) => Transition::Suppressed,
        (None, AlertPhase::Alerted) => {
            *phase = AlertPhase::Idle;
            Transition::Event(CriticalEventKind::Recovered)
        }
        // Recovered while a Failure dispatch was still in flight: wait for
        // `record_alert_outcome` to resolve `Pending`, then the *next*
        // report (now healthy) will see `(None, Alerted)` above and fire
        // the Recovered notice — or `(None, Idle)` if delivery failed, and
        // stay silent, matching "the failure was never actually reported."
        _ => Transition::None,
    }
}

enum Transition {
    None,
    Suppressed,
    Event(CriticalEventKind),
}

/// Builds the event, dispatches it, and — for `Failure` events only — feeds
/// the delivery outcome back into the stored phase so a failed send is
/// retried on the next unhealthy report rather than silently left `Pending`
/// forever (Greptile P1). `Recovered` events commit their phase transition
/// synchronously in `decide_transition` regardless of delivery outcome: a
/// lost "all clear" is far less costly than a lost failure notice, and
/// keeping it synchronous avoids a second class of pending-state to track.
///
/// `generation` is the `*_unhealthy_since` this `Failure` transition was
/// decided against (`None`/unused for `Recovered`) — passed through to
/// `record_alert_outcome` so a callback that resolves after a newer
/// incident has already started can tell it's stale.
fn dispatch_transition(
    key: (AgentKind, String),
    category: AlertCategory,
    kind: CriticalEventKind,
    generation: Option<Instant>,
) {
    let (Some(cfg), Some(client)) = (ALERTS_CONFIG.get(), ALERTS_CLIENT.get()) else {
        return;
    };
    let cfg = cfg.clone();
    let client = client.clone();
    let description = match (category, kind) {
        (AlertCategory::RegistrationLoss, CriticalEventKind::Failure) => format!(
            "{} line unregistered for over {}s",
            key.0.as_str(),
            cfg.registration_loss_thresholds.unhealthy_sec
        ),
        (AlertCategory::RegistrationLoss, CriticalEventKind::Recovered) => {
            format!("{} line re-registered", key.0.as_str())
        }
        (AlertCategory::TunnelFailure, CriticalEventKind::Failure) => format!(
            "{} line's tunnel non-established for over {}s",
            key.0.as_str(),
            cfg.tunnel_failure_thresholds.unhealthy_sec
        ),
        (AlertCategory::TunnelFailure, CriticalEventKind::Recovered) => {
            format!("{} line's tunnel re-established", key.0.as_str())
        }
        (AlertCategory::GmConnectionLost, CriticalEventKind::Failure) => format!(
            "{} line's carrier signaling connection down for over {}s",
            key.0.as_str(),
            cfg.gm_connection_lost_thresholds.unhealthy_sec
        ),
        (AlertCategory::GmConnectionLost, CriticalEventKind::Recovered) => {
            format!(
                "{} line's carrier signaling connection re-established",
                key.0.as_str()
            )
        }
        _ => unreachable!(
            "only RegistrationLoss/TunnelFailure/GmConnectionLost transitions are produced here"
        ),
    };
    let phone_number = ALERTS_PHONE_MAP.get().and_then(|m| m.get(&key.1).cloned());
    let event = CriticalEvent {
        category,
        unit_id: Some(key.1.clone()),
        description,
        phone_number,
        at: chrono::Utc::now(),
        kind,
    };

    tokio::spawn(async move {
        let outcome = crate::alerts::dispatch(&client, &cfg, event).await;
        if kind == CriticalEventKind::Failure {
            let success = matches!(outcome, AlertOutcome::Sent(_));
            // `generation` is always `Some` here — only `Failure`
            // transitions reach this branch, and `decide_transition` only
            // ever produces one from `Some(since)`.
            if let Some(generation) = generation {
                record_alert_outcome(&key, category, generation, success);
            }
        }
    });
}

/// Resolves a `Pending` phase once its dispatch completes: `Alerted` on
/// confirmed delivery, back to `Idle` on failure so the next unhealthy
/// report retries instead of the incident staying silently suppressed
/// forever (Greptile P1). A no-op if the phase has since moved on, *or* if
/// `generation` no longer matches the record's current `*_unhealthy_since`
/// — the latter means this callback is for an incident that has already
/// ended (health recovered and, possibly, went unhealthy again) since the
/// dispatch was spawned, and applying it would corrupt a newer incident's
/// state instead of the stale one it actually belongs to (Greptile P1
/// follow-up: "delivery callbacks not scoped to an incident generation").
fn record_alert_outcome(
    key: &(AgentKind, String),
    category: AlertCategory,
    generation: Instant,
    success: bool,
) {
    let mut guard = liveness().lock().unwrap();
    let Some(record) = guard.get_mut(key) else {
        return;
    };
    let (phase, unhealthy_since) = match category {
        AlertCategory::RegistrationLoss => (
            &mut record.registered_alert_phase,
            record.registered_unhealthy_since,
        ),
        AlertCategory::TunnelFailure => (
            &mut record.tunnel_alert_phase,
            record.tunnel_unhealthy_since,
        ),
        AlertCategory::GmConnectionLost => (
            &mut record.gm_connection_alert_phase,
            record.gm_connection_unhealthy_since,
        ),
        _ => return,
    };
    if *phase == AlertPhase::Pending && unhealthy_since == Some(generation) {
        *phase = if success {
            AlertPhase::Alerted
        } else {
            AlertPhase::Idle
        };
    }
}

fn apply_state(
    agent: AgentKind,
    module_id: &str,
    state: &AgentState,
    registered_unhealthy_since: &mut Option<Instant>,
    tunnel_unhealthy_since: &mut Option<Instant>,
    gm_connection_unhealthy_since: &mut Option<Instant>,
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
    // specs/028-gm-tcp-reconnect. `None` here means the agent did not report
    // the signal (an older peer, or a partial report), and MUST leave both the
    // gauge and the unhealthy timer untouched — reading absent as "down" would
    // report every line down on any report that happens not to carry it.
    if let Some(gm_connection_up) = state.gm_connection_up {
        let up = if gm_connection_up { 1.0 } else { 0.0 };
        match agent {
            // Both the VoWiFi and VoLTE paths file under the one gauge, keyed
            // by `module` — see the metric's own doc for why the `vowifi_`
            // prefix is retained for VoLTE.
            AgentKind::Ims | AgentKind::Sip | AgentKind::Volte => metrics::VOWIFI_GM_CONNECTION_UP
                .with_label_values(&[module_id])
                .set(up),
            // The telephony half carries no Gm connection of its own.
            AgentKind::VolteSip => {}
        }
        if gm_connection_up {
            *gm_connection_unhealthy_since = None;
        } else if gm_connection_unhealthy_since.is_none() {
            *gm_connection_unhealthy_since = Some(Instant::now());
        }
    }
    // pbx_registered (Agent B) has no dedicated gauge yet — sip_registered
    // remains the daemon's own PBX registration (metrics-inventory.md
    // "Unchanged" note); tracked here only so liveness has somewhere to
    // record Agent B reported it, for future use.

    // SIP-server mode: the registrar is hosted by whichever process owns the
    // SIP side, and on the VoWiFi/VoLTE paths that is a telephony agent with no
    // `/metrics` of its own. Only this ingest path can put those numbers on the
    // daemon's endpoint, which is what FR-022 actually requires — verified
    // missing on a live container before this existed (spec 024).
    //
    // Unlabelled on purpose: there is exactly one registrar per deployment, so a
    // per-line label would report the same value N times and invite an operator
    // to sum it.
    if let Some(bindings) = state.sip_server_bindings {
        metrics::SIP_SERVER_BINDINGS.set(f64::from(bindings));
    }
    if let Some(ring_registered) = state.sip_server_ring_registered {
        metrics::SIP_SERVER_RING_AOR_REGISTERED.set(if ring_registered { 1.0 } else { 0.0 });
    }
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
        ObservedEvent::OutboundAttempt { outcome } => {
            metrics::OUTBOUND_ATTEMPTS_TOTAL
                .with_label_values(&[outcome.as_str()])
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

    /// FR-022 across a process boundary. The registrar is hosted by whichever
    /// process owns the SIP side, and on the VoWiFi/VoLTE paths that is a
    /// telephony agent with no `/metrics` — so gauges set in that process are
    /// never scraped. A live container showed exactly that: a registered phone,
    /// and zero `sip_server` series on the daemon's endpoint. This ingest path is
    /// the only thing that puts them there.
    #[test]
    fn a_hosted_registrars_state_reaches_the_daemons_gauges() {
        apply_report(&AgentReport {
            agent: AgentKind::Sip,
            module_id: "sip-server".to_string(),
            epoch: 9101,
            seq: 1,
            state: AgentState {
                sip_server_bindings: Some(3),
                sip_server_ring_registered: Some(true),
                ..Default::default()
            },
            events: Vec::new(),
            dropped: 0,
        });

        assert_eq!(metrics::SIP_SERVER_BINDINGS.get(), 3.0);
        assert_eq!(metrics::SIP_SERVER_RING_AOR_REGISTERED.get(), 1.0);

        // And the "phone went away" direction, which is the one an operator
        // alerts on.
        apply_report(&AgentReport {
            agent: AgentKind::Sip,
            module_id: "sip-server".to_string(),
            epoch: 9101,
            seq: 2,
            state: AgentState {
                sip_server_bindings: Some(0),
                sip_server_ring_registered: Some(false),
                ..Default::default()
            },
            events: Vec::new(),
            dropped: 0,
        });

        assert_eq!(metrics::SIP_SERVER_BINDINGS.get(), 0.0);
        assert_eq!(metrics::SIP_SERVER_RING_AOR_REGISTERED.get(), 0.0);

        // And an agent that hosts no registrar must leave these alone rather
        // than reporting a default and zeroing a healthy deployment.
        //
        // Asserted in this same test on purpose: unlike every other gauge here
        // these two are unlabelled singletons, so two tests touching them would
        // race under a parallel runner — which is exactly what happened when
        // this was written as a second `#[test]`.
        metrics::SIP_SERVER_BINDINGS.set(7.0);
        metrics::SIP_SERVER_RING_AOR_REGISTERED.set(1.0);
        apply_report(&AgentReport {
            agent: AgentKind::Ims,
            module_id: "test-ingest-no-registrar".to_string(),
            epoch: 9102,
            seq: 1,
            state: AgentState {
                registered: Some(true),
                ..Default::default()
            },
            events: Vec::new(),
            dropped: 0,
        });
        assert_eq!(metrics::SIP_SERVER_BINDINGS.get(), 7.0);
        assert_eq!(metrics::SIP_SERVER_RING_AOR_REGISTERED.get(), 1.0);
    }

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
    // `ALERTS_CONFIG`/`ALERTS_CLIENT` are deliberately never initialized
    // anywhere in this lib's unit-test binary: both are process-global
    // `OnceLock`s shared by every `#[cfg(test)]` module in the crate, and
    // once set, `apply_report` starts calling `tokio::spawn` on a
    // threshold crossing — which panics outside a Tokio runtime, and every
    // test here is a plain `#[test]`, not `#[tokio::test]`. The
    // phase-transition and delivery-outcome logic below is therefore
    // tested directly against the pure `decide_transition`/
    // `record_alert_outcome` functions instead of through `apply_report`;
    // the full dispatch-and-retry path is covered end-to-end by
    // `tests/test_alerts_discord.rs` (a separate binary, free to call
    // `init_alerts` and use `#[tokio::test]`).

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
        assert_eq!(record.registered_alert_phase, AlertPhase::Idle);
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
    fn decide_transition_fires_failure_once_threshold_crossed() {
        let mut phase = AlertPhase::Idle;
        let since = Instant::now() - Duration::from_secs(301);
        let t = decide_transition(Some(since), &mut phase, Duration::from_secs(300));
        assert!(matches!(t, Transition::Event(CriticalEventKind::Failure)));
        assert_eq!(
            phase,
            AlertPhase::Pending,
            "moves to Pending, not straight to Alerted — Alerted is only \
             reached once record_alert_outcome confirms delivery (Greptile P1)"
        );
    }

    #[test]
    fn decide_transition_does_not_refire_while_dispatch_pending() {
        // A second unhealthy report arriving while the first Failure
        // dispatch is still in flight must not spawn a second, overlapping
        // send.
        let mut phase = AlertPhase::Pending;
        let since = Instant::now() - Duration::from_secs(301);
        let t = decide_transition(Some(since), &mut phase, Duration::from_secs(300));
        assert!(matches!(t, Transition::None));
        assert_eq!(phase, AlertPhase::Pending);
    }

    #[test]
    fn decide_transition_suppresses_while_alerted_and_still_unhealthy() {
        let mut phase = AlertPhase::Alerted;
        let since = Instant::now() - Duration::from_secs(301);
        let t = decide_transition(Some(since), &mut phase, Duration::from_secs(300));
        assert!(
            matches!(t, Transition::Suppressed),
            "must not re-alert while continuously unhealthy (FR-013)"
        );
        assert_eq!(phase, AlertPhase::Alerted);
    }

    #[test]
    fn decide_transition_fires_recovered_once_after_alerted() {
        let mut phase = AlertPhase::Alerted;
        let t = decide_transition(None, &mut phase, Duration::from_secs(300));
        assert!(matches!(t, Transition::Event(CriticalEventKind::Recovered)));
        assert_eq!(phase, AlertPhase::Idle);

        // And exactly once — a further healthy call emits nothing more.
        let t_again = decide_transition(None, &mut phase, Duration::from_secs(300));
        assert!(matches!(t_again, Transition::None));
    }

    #[test]
    fn decide_transition_no_event_when_recovered_before_threshold() {
        // Only 30s elapsed, well under a 300s threshold — a self-healed blip
        // must never alert.
        let mut phase = AlertPhase::Idle;
        let since = Instant::now() - Duration::from_secs(30);
        let t = decide_transition(Some(since), &mut phase, Duration::from_secs(300));
        assert!(matches!(t, Transition::None));
        assert_eq!(phase, AlertPhase::Idle);
    }

    #[test]
    fn decide_transition_stays_pending_when_recovered_mid_dispatch() {
        // Health recovers while a Failure dispatch is still in flight: no
        // event yet (the callback hasn't resolved), phase left exactly as
        // record_alert_outcome will find it.
        let mut phase = AlertPhase::Pending;
        let t = decide_transition(None, &mut phase, Duration::from_secs(300));
        assert!(matches!(t, Transition::None));
        assert_eq!(phase, AlertPhase::Pending);
    }

    /// specs/022-discord-critical-alerts Greptile P1 ("Failed delivery
    /// suppresses incident"): confirmed delivery moves Pending -> Alerted.
    #[test]
    fn record_alert_outcome_moves_pending_to_alerted_on_success() {
        let module_id = "test-outcome-success";
        let key = (AgentKind::Ims, module_id.to_string());
        let generation = seed_pending_record(&key);

        record_alert_outcome(&key, AlertCategory::RegistrationLoss, generation, true);

        let guard = liveness().lock().unwrap();
        assert_eq!(
            guard.get(&key).unwrap().registered_alert_phase,
            AlertPhase::Alerted
        );
    }

    /// The other half of the same fix: a failed delivery resets to `Idle`
    /// rather than leaving the incident stuck — so the *next* unhealthy
    /// report (`decide_transition` above) retries instead of the failure
    /// notification being permanently lost while a later recovery still
    /// fires a "recovered" notice for a failure nobody was ever told about.
    #[test]
    fn record_alert_outcome_resets_pending_to_idle_on_failure() {
        let module_id = "test-outcome-failure";
        let key = (AgentKind::Ims, module_id.to_string());
        let generation = seed_pending_record(&key);

        record_alert_outcome(&key, AlertCategory::RegistrationLoss, generation, false);

        let guard = liveness().lock().unwrap();
        assert_eq!(
            guard.get(&key).unwrap().registered_alert_phase,
            AlertPhase::Idle,
            "a failed delivery must be retryable, not permanently suppressed"
        );
    }

    #[test]
    fn record_alert_outcome_is_a_noop_if_phase_already_moved_on() {
        // Guards against a stale callback (e.g. a slow dispatch resolving
        // after something else already changed the phase) clobbering
        // newer state.
        let module_id = "test-outcome-stale-callback";
        let key = (AgentKind::Ims, module_id.to_string());
        let generation = seed_pending_record(&key);
        record_alert_outcome(&key, AlertCategory::RegistrationLoss, generation, true); // Pending -> Alerted

        record_alert_outcome(&key, AlertCategory::RegistrationLoss, generation, false); // stale, must not fire

        let guard = liveness().lock().unwrap();
        assert_eq!(
            guard.get(&key).unwrap().registered_alert_phase,
            AlertPhase::Alerted
        );
    }

    /// specs/022-discord-critical-alerts Greptile P1 follow-up ("delivery
    /// callbacks not scoped to an incident generation"): a callback for an
    /// *old* incident (its `unhealthy_since` no longer matches the record's
    /// current one — health recovered and, here, went unhealthy again in
    /// the meantime) must not touch the phase of the new incident at all.
    #[test]
    fn record_alert_outcome_ignores_a_stale_generation_from_an_earlier_incident() {
        let module_id = "test-outcome-stale-generation";
        let key = (AgentKind::Ims, module_id.to_string());
        let old_generation = seed_pending_record(&key); // incident A: Pending

        // Incident A recovers, then a fresh incident B starts and is
        // already Pending on its own (new) generation — exactly what
        // `apply_report` would have produced via decide_transition.
        let new_generation = Instant::now();
        {
            let mut guard = liveness().lock().unwrap();
            let record = guard.get_mut(&key).unwrap();
            record.registered_unhealthy_since = Some(new_generation);
            record.registered_alert_phase = AlertPhase::Pending;
        }
        assert_ne!(old_generation, new_generation);

        // Incident A's (stale) dispatch finally resolves.
        record_alert_outcome(&key, AlertCategory::RegistrationLoss, old_generation, true);

        // Incident B's phase must be untouched — still Pending, not
        // wrongly marked Alerted for a Failure it never actually had sent.
        let guard = liveness().lock().unwrap();
        let record = guard.get(&key).unwrap();
        assert_eq!(
            record.registered_alert_phase,
            AlertPhase::Pending,
            "a stale callback for a previous incident must not resolve the current one"
        );
        assert_eq!(record.registered_unhealthy_since, Some(new_generation));
    }

    /// Returns the seeded `registered_unhealthy_since` — the generation a
    /// caller must pass to `record_alert_outcome` for it to actually apply.
    fn seed_pending_record(key: &(AgentKind, String)) -> Instant {
        let generation = Instant::now();
        let mut guard = liveness().lock().unwrap();
        guard.insert(
            key.clone(),
            AgentRecord {
                last_report: Instant::now(),
                last_applied: (0, 0),
                registered_unhealthy_since: Some(generation),
                registered_alert_phase: AlertPhase::Pending,
                tunnel_unhealthy_since: None,
                tunnel_alert_phase: AlertPhase::Idle,
                gm_connection_unhealthy_since: None,
                gm_connection_alert_phase: AlertPhase::Idle,
            },
        );
        generation
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
    fn tunnel_unhealthy_since_tracked_independently_of_registration() {
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

        let guard = liveness().lock().unwrap();
        let record = guard.get(&(AgentKind::Ims, module_id.to_string())).unwrap();
        assert!(
            record.registered_unhealthy_since.is_none(),
            "registered=true must not be treated as unhealthy"
        );
        assert!(
            record.tunnel_unhealthy_since.is_some(),
            "tunnel_up=false must be tracked independently"
        );
    }
}
