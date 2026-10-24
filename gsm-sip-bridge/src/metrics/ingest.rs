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
    AgentDetails, AgentKind, AgentReport, AgentState, CallStatus, IkeSaLossCause, ObservedEvent,
    QualitySource, RegistrationStateWire, SmsOutcome, TunnelRecoveryReason, UplinkState,
};
use crate::metrics;
use crate::store::events::{self, EventKind};
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
        // The supervisor never reports calls; vowifi is its only path.
        AgentKind::Ims | AgentKind::Sip | AgentKind::Supervisor => TRANSPORT_VOWIFI,
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
    /// The `Failure` in flight for `registered_alert_phase` (#138).
    registered_in_flight: FailureInFlight,
    /// Same pair, for `tunnel_up`.
    tunnel_unhealthy_since: Option<Instant>,
    tunnel_alert_phase: AlertPhase,
    /// specs/086. State of the folded tunnel decision; only ever set on the
    /// supervisor's record, which owns it. See [`FailureInFlight`].
    tunnel_fold: FailureInFlight,
    /// specs/087. Supervisor records only: the supervisor's last-reported
    /// `tunnel_flapping`. A flapping tunnel counts as unhealthy even while the
    /// CHILD_SA is up, so a line rebuilt every few minutes still alerts.
    tunnel_flapping: bool,
    /// The most recent IKE_SA loss cause the supervisor reported, for alert text.
    last_ike_sa_loss_cause: Option<IkeSaLossCause>,
    /// The supervisor's last-reported uplink state (specs/100), for alert text.
    last_uplink: Option<UplinkState>,
    /// Whether the open tunnel streak was opened by flapping (backdated by the
    /// threshold so it alerts at once) rather than by an outage. Picks the alert
    /// wording.
    streak_opened_by_flapping: bool,
    /// Same pair, for `gm_connection_up` (specs/028-gm-tcp-reconnect).
    gm_connection_unhealthy_since: Option<Instant>,
    gm_connection_alert_phase: AlertPhase,
    /// The `Failure` in flight for `gm_connection_alert_phase` (#138).
    gm_connection_in_flight: FailureInFlight,
    /// When this line's registration lapses, as reported by the agent
    /// (specs/039-at-stall-watchdog). Absolute, so the countdown can be
    /// computed fresh at every scrape without the agent reporting per second.
    /// `None` until an agent reports one.
    registration_expires_at: Option<u64>,
    /// specs/097. Supervisor records only: the open pool-exhaustion episode.
    pool_exhausted: PoolExhaustion,
    /// When the daemon first saw a report from this record's current `epoch`
    /// (unix seconds): roughly when that agent process started. A
    /// pool-exhaustion episode ends only on a registration from an agent first
    /// seen after the rebuild (specs/097).
    epoch_first_seen_unix: Option<u64>,
}

/// One line's pool-exhaustion episode (specs/097 data-model.md): from its first
/// `PcscfPoolExhausted` rebuild until an IMS agent started after the latest one
/// registers. The addresses for the alert text live in
/// [`pool_exhausted_pcscfs`], since this record is `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PoolExhaustion {
    /// When the first rebuild of the open episode was reported; `None` when no
    /// episode is open. The alert's unhealthy-since.
    since: Option<Instant>,
    phase: AlertPhase,
    /// Rebuilds reported in the open episode.
    rebuilds: u32,
    /// When the supervisor issued the latest rebuild (unix seconds, from the
    /// event itself). The agent it replaced kept heartbeating
    /// `registered = true`; only an agent first seen after this time can end
    /// the episode, whatever order the two reports arrive in (research R5).
    rebuilt_at_unix: Option<u64>,
    /// The episode start a `Failure` was decided against, frozen so its
    /// delivery callback still matches after the episode has ended.
    generation: Option<Instant>,
    /// The episode ended while that `Failure` was still being sent: the
    /// callback owes the operator an all-clear (as [`FailureInFlight::ended`]).
    ended: bool,
}

impl Default for PoolExhaustion {
    fn default() -> Self {
        Self {
            since: None,
            phase: AlertPhase::Idle,
            rebuilds: 0,
            rebuilt_at_unix: None,
            generation: None,
            ended: false,
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Ends `episode` for a registration from an agent first seen at
/// `agent_first_seen`, unless that agent predates the latest rebuild.
fn end_pool_exhaustion_if_replaced(episode: &mut PoolExhaustion, agent_first_seen: Option<u64>) {
    if episode.since.is_none() {
        return;
    }
    let replaced = match (episode.rebuilt_at_unix, agent_first_seen) {
        (Some(rebuilt), Some(seen)) => seen > rebuilt,
        // No rebuild time to compare against: any registration ends it.
        (None, _) => true,
        (Some(_), None) => false,
    };
    if replaced {
        episode.since = None;
        // A `Failure` still in flight is owed an all-clear by its callback.
        if episode.phase == AlertPhase::Pending {
            episode.ended = true;
        }
    }
}

/// `module_id ->` the P-CSCFs the latest `PcscfPoolExhausted` rebuild named,
/// for the alert text only.
fn pool_exhausted_pcscfs() -> &'static Mutex<HashMap<String, Vec<String>>> {
    static PCSCFS: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    PCSCFS.get_or_init(|| Mutex::new(HashMap::new()))
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

/// `(owning key, category, transition, generation)`. The key is explicit because
/// a folded tunnel decision (specs/086) is owned by the supervisor's record even
/// when an IMS report triggered it.
type PendingTransition = (
    (AgentKind, String),
    AlertCategory,
    CriticalEventKind,
    Option<Instant>,
);

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
    let transitions = apply_report_with(report, ALERTS_CONFIG.get());
    for (key, category, kind, generation) in transitions {
        dispatch_transition(key, category, kind, generation);
    }
}

/// [`apply_report`] with the alert configuration injected, returning the
/// transitions to dispatch rather than dispatching them -- so the two-reporter
/// alert wiring can be exercised without the process-wide `ALERTS_CONFIG` or a
/// Discord client.
fn apply_report_with(
    report: &AgentReport,
    alerts: Option<&AlertsConfig>,
) -> Vec<PendingTransition> {
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
    let mut registered_in_flight =
        existing.map_or_else(FailureInFlight::default, |r| r.registered_in_flight);
    let mut gm_connection_in_flight =
        existing.map_or_else(FailureInFlight::default, |r| r.gm_connection_in_flight);
    let mut tunnel_alert_phase = existing.map_or(AlertPhase::Idle, |r| r.tunnel_alert_phase);
    let mut tunnel_fold = existing.map_or_else(FailureInFlight::default, |r| r.tunnel_fold);
    let mut tunnel_flapping = existing.is_some_and(|r| r.tunnel_flapping);
    let mut last_ike_sa_loss_cause = existing.and_then(|r| r.last_ike_sa_loss_cause);
    let mut last_uplink = existing.and_then(|r| r.last_uplink);
    let mut streak_opened_by_flapping = existing.is_some_and(|r| r.streak_opened_by_flapping);
    let mut gm_connection_alert_phase =
        existing.map_or(AlertPhase::Idle, |r| r.gm_connection_alert_phase);
    let mut registered_unhealthy_since = existing.and_then(|r| r.registered_unhealthy_since);
    let mut tunnel_unhealthy_since = existing.and_then(|r| r.tunnel_unhealthy_since);
    let mut gm_connection_unhealthy_since = existing.and_then(|r| r.gm_connection_unhealthy_since);
    let mut registration_expires_at = existing.and_then(|r| r.registration_expires_at);
    let mut pool_exhausted = existing.map_or_else(PoolExhaustion::default, |r| r.pool_exhausted);
    // A new epoch is a new agent process; its first report dates it.
    let epoch_first_seen_unix = match existing {
        Some(r) if r.last_applied.0 == report.epoch => r.epoch_first_seen_unix,
        _ => Some(now_unix()),
    };
    // (category, transition, generation) to dispatch once the lock is
    // released. `generation` is the `*_unhealthy_since` value this
    // particular transition was decided against — threaded through to
    // `record_alert_outcome` so a delivery callback that resolves after a
    // *later* incident has already started can recognize it's stale and
    // become a no-op instead of corrupting the new incident's phase
    // (Greptile P1 follow-up: "delivery callbacks not scoped to an incident
    // generation").
    let mut pending_transitions: Vec<PendingTransition> = Vec::new();

    if !is_replay {
        apply_state(
            report.agent,
            module_id,
            &report.state,
            &mut registered_unhealthy_since,
            &mut tunnel_unhealthy_since,
            &mut gm_connection_unhealthy_since,
            &mut registration_expires_at,
        );

        if report.agent == AgentKind::Supervisor {
            note_pool_exhausted_rebuilds(&mut pool_exhausted, module_id, &report.events);
        } else if report.agent == AgentKind::Ims && report.state.registered == Some(true) {
            // The line registered: that ends its supervisor's episode, unless
            // this agent predates the latest rebuild.
            if let Some(sup) = guard.get_mut(&(AgentKind::Supervisor, module_id.to_string())) {
                end_pool_exhaustion_if_replaced(&mut sup.pool_exhausted, epoch_first_seen_unix);
            }
        }

        if report.agent == AgentKind::Supervisor {
            if let Some(cause) = report.state.ike_sa_loss_cause {
                last_ike_sa_loss_cause = Some(cause);
            }
            if let Some(uplink) = report.state.uplink {
                last_uplink = Some(uplink);
                metrics::VOWIFI_UPLINK_HELD
                    .with_label_values(&[module_id])
                    .set(if uplink == UplinkState::Up { 0.0 } else { 1.0 });
            }
            if let Some(flapping) = report.state.tunnel_flapping {
                tunnel_flapping = flapping;
            }
            fold_flapping_into_tunnel_streak(
                &mut tunnel_unhealthy_since,
                existing.and_then(|r| r.tunnel_unhealthy_since),
                tunnel_flapping,
                &report.state,
                alerts.map(|c| Duration::from_secs(c.tunnel_failure_thresholds.unhealthy_sec)),
                &mut streak_opened_by_flapping,
            );
        }

        for event in &report.events {
            apply_event(report.agent, module_id, event);
        }

        if report.dropped > 0 {
            metrics::OBSERVABILITY_EVENTS_DROPPED_TOTAL
                .with_label_values(&[report.agent.as_str(), module_id])
                .inc_by(report.dropped as f64);
        }

        if let Some(cfg) = alerts {
            let registration_threshold =
                Duration::from_secs(cfg.registration_loss_thresholds.unhealthy_sec);
            let tunnel_threshold = Duration::from_secs(cfg.tunnel_failure_thresholds.unhealthy_sec);

            // The web UI's timeline resolves an incident when the condition
            // clears, whether or not a Discord notice was ever sent for it.
            // The Discord path only sends `Recovered` after a delivered
            // `Failure`, so with a category disabled, no webhook, or a failed
            // delivery its raise would otherwise stay open for ever and fold
            // every later outage into it. A resolve with nothing open is a
            // no-op, so this is safe to call on every healthy report.
            let supervisor_key_tl = (AgentKind::Supervisor, module_id.to_string());
            let sup_since_tl = guard
                .get(&supervisor_key_tl)
                .and_then(|r| r.tunnel_unhealthy_since);
            let ims_since_tl = guard
                .get(&(AgentKind::Ims, module_id.to_string()))
                .and_then(|r| r.tunnel_unhealthy_since);
            // Only the agent that owns a signal can say it has cleared: the
            // SIP-side agents never report registration or tunnel state, so
            // their heartbeats would otherwise read as "healthy".
            let carrier = matches!(report.agent, AgentKind::Ims | AgentKind::Volte);
            let tunnel_healthy = match report.agent {
                AgentKind::Supervisor => ims_since_tl.is_none() && tunnel_unhealthy_since.is_none(),
                AgentKind::Ims => tunnel_unhealthy_since.is_none() && sup_since_tl.is_none(),
                _ => false,
            };
            for (category, healthy) in [
                (
                    AlertCategory::RegistrationLoss,
                    carrier && registered_unhealthy_since.is_none(),
                ),
                (AlertCategory::TunnelFailure, tunnel_healthy),
                (
                    AlertCategory::GmConnectionLost,
                    carrier && gm_connection_unhealthy_since.is_none(),
                ),
                (
                    AlertCategory::PcscfPoolExhausted,
                    report.agent == AgentKind::Supervisor && pool_exhausted.since.is_none(),
                ),
            ] {
                if healthy {
                    events::record_alert(
                        category.as_str(),
                        Some(module_id),
                        false,
                        "condition cleared",
                        chrono::Utc::now(),
                    );
                }
            }

            match decide_tracked(
                registered_unhealthy_since,
                &mut registered_alert_phase,
                &mut registered_in_flight,
                registration_threshold,
            ) {
                Transition::Event(kind) => pending_transitions.push((
                    key.clone(),
                    AlertCategory::RegistrationLoss,
                    kind,
                    registered_in_flight.generation,
                )),
                Transition::Suppressed => {
                    crate::alerts::record_suppressed(AlertCategory::RegistrationLoss)
                }
                Transition::None => {}
            }
            // specs/086: with a supervisor reporting this module, the tunnel
            // alert fires on either the IMS agent's tunnel signal or the
            // supervisor's CHILD_SA signal, once per outage. The supervisor's
            // record owns the folded decision.
            let supervisor_key = (AgentKind::Supervisor, module_id.to_string());
            let folds = matches!(report.agent, AgentKind::Ims | AgentKind::Supervisor)
                && (report.agent == AgentKind::Supervisor || guard.contains_key(&supervisor_key));
            if folds && report.agent == AgentKind::Supervisor {
                let ims_since = guard
                    .get(&(AgentKind::Ims, module_id.to_string()))
                    .and_then(|r| r.tunnel_unhealthy_since);
                match decide_folded_tunnel(
                    ims_since,
                    tunnel_unhealthy_since,
                    &mut tunnel_alert_phase,
                    &mut tunnel_fold,
                    tunnel_threshold,
                ) {
                    Transition::Event(kind) => pending_transitions.push((
                        key.clone(),
                        AlertCategory::TunnelFailure,
                        kind,
                        tunnel_fold.generation,
                    )),
                    Transition::Suppressed => {
                        crate::alerts::record_suppressed(AlertCategory::TunnelFailure)
                    }
                    Transition::None => {}
                }
            } else if folds {
                // An IMS report: decide on the supervisor's record.
                let sup = guard.get_mut(&supervisor_key).unwrap();
                match decide_folded_tunnel(
                    tunnel_unhealthy_since,
                    sup.tunnel_unhealthy_since,
                    &mut sup.tunnel_alert_phase,
                    &mut sup.tunnel_fold,
                    tunnel_threshold,
                ) {
                    Transition::Event(kind) => pending_transitions.push((
                        supervisor_key.clone(),
                        AlertCategory::TunnelFailure,
                        kind,
                        sup.tunnel_fold.generation,
                    )),
                    Transition::Suppressed => {
                        crate::alerts::record_suppressed(AlertCategory::TunnelFailure)
                    }
                    Transition::None => {}
                }
            } else {
                match decide_transition(
                    tunnel_unhealthy_since,
                    &mut tunnel_alert_phase,
                    tunnel_threshold,
                ) {
                    Transition::Event(kind) => pending_transitions.push((
                        key.clone(),
                        AlertCategory::TunnelFailure,
                        kind,
                        tunnel_unhealthy_since,
                    )),
                    Transition::Suppressed => {
                        crate::alerts::record_suppressed(AlertCategory::TunnelFailure)
                    }
                    Transition::None => {}
                }
            }
            let gm_connection_threshold =
                Duration::from_secs(cfg.gm_connection_lost_thresholds.unhealthy_sec);
            match decide_tracked(
                gm_connection_unhealthy_since,
                &mut gm_connection_alert_phase,
                &mut gm_connection_in_flight,
                gm_connection_threshold,
            ) {
                Transition::Event(kind) => pending_transitions.push((
                    key.clone(),
                    AlertCategory::GmConnectionLost,
                    kind,
                    gm_connection_in_flight.generation,
                )),
                Transition::Suppressed => {
                    crate::alerts::record_suppressed(AlertCategory::GmConnectionLost)
                }
                Transition::None => {}
            }
            // Decided on the supervisor's own reports, which heartbeat, so an
            // episode ended by an IMS report is all-cleared one interval later.
            if report.agent == AgentKind::Supervisor {
                let threshold =
                    Duration::from_secs(cfg.pcscf_pool_exhausted_thresholds.unhealthy_sec);
                match decide_transition(pool_exhausted.since, &mut pool_exhausted.phase, threshold)
                {
                    Transition::Event(kind) => {
                        if kind == CriticalEventKind::Failure {
                            pool_exhausted.generation = pool_exhausted.since;
                            pool_exhausted.ended = false;
                        } else {
                            pool_exhausted.generation = None;
                        }
                        pending_transitions.push((
                            key.clone(),
                            AlertCategory::PcscfPoolExhausted,
                            kind,
                            pool_exhausted.generation,
                        ))
                    }
                    Transition::Suppressed => {
                        crate::alerts::record_suppressed(AlertCategory::PcscfPoolExhausted)
                    }
                    Transition::None => {}
                }
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
            registered_in_flight,
            tunnel_unhealthy_since,
            tunnel_alert_phase,
            tunnel_fold,
            tunnel_flapping,
            last_ike_sa_loss_cause,
            last_uplink,
            streak_opened_by_flapping,
            gm_connection_unhealthy_since,
            gm_connection_alert_phase,
            gm_connection_in_flight,
            registration_expires_at,
            pool_exhausted,
            epoch_first_seen_unix,
        },
    );
    drop(guard);
    if !is_replay {
        store_view(report);
        export_signal(report);
    }
    pending_transitions
}

/// The newest value each field of an agent's `AgentState` carried, plus its
/// `AgentDetails`, for the management web UI (specs/122-management-webui).
///
/// Kept apart from `AgentRecord`, which is `Copy` and read with `.copied()`
/// all through the alert state machine; this owns strings and a list. A
/// `Some` in a report overwrites, a `None` leaves what was there (the same
/// rule the gauges follow).
#[derive(Debug, Clone, Default)]
struct StoredView {
    registered: Option<bool>,
    tunnel_up: Option<bool>,
    gm_connection_up: Option<bool>,
    child_sa_up: Option<bool>,
    pbx_registered: Option<bool>,
    active_calls: Option<u32>,
    details: AgentDetails,
}

fn details() -> &'static Mutex<HashMap<(AgentKind, String), StoredView>> {
    static DETAILS: OnceLock<Mutex<HashMap<(AgentKind, String), StoredView>>> = OnceLock::new();
    DETAILS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_view(report: &AgentReport) {
    let mut guard = details().lock().unwrap();
    let view = guard
        .entry((report.agent, report.module_id.clone()))
        .or_default();
    let st = &report.state;
    view.registered = st.registered.or(view.registered);
    view.tunnel_up = st.tunnel_up.or(view.tunnel_up);
    view.gm_connection_up = st.gm_connection_up.or(view.gm_connection_up);
    view.child_sa_up = st.child_sa_up.or(view.child_sa_up);
    view.pbx_registered = st.pbx_registered.or(view.pbx_registered);
    view.active_calls = st.active_calls.or(view.active_calls);
    if let Some(d) = &report.details {
        let kept = &mut view.details;
        if d.registration_state.is_some() {
            kept.registration_state = d.registration_state;
            // "Registered since" only means something while registered. A
            // report that names a lapsed state clears it; one that omits the
            // state (an older agent) leaves it alone.
            if !matches!(
                d.registration_state,
                Some(RegistrationStateWire::Registered | RegistrationStateWire::Renewing)
            ) {
                kept.registered_at = None;
            }
        }
        if d.registered_at.is_some() {
            kept.registered_at = d.registered_at;
        }
        if d.plmn.is_some() {
            kept.plmn = d.plmn.clone();
        }
        if d.signal.is_some() {
            kept.signal = d.signal.clone();
        }
        // Replaces rather than merges: `Some(vec![])` is "no calls".
        if d.active_call_details.is_some() {
            kept.active_call_details = d.active_call_details.clone();
        }
    }
}

/// Exports a modem-backed line's reading as the `signal_*` gauges
/// (specs/122-management-webui). Only the carrier side reads the modem.
fn export_signal(report: &AgentReport) {
    if !matches!(report.agent, AgentKind::Ims | AgentKind::Volte) {
        return;
    }
    if let Some(wire) = report.details.as_ref().and_then(|d| d.signal.clone()) {
        metrics::set_signal(
            &report.module_id,
            transport_label(report.agent),
            &wire.into(),
        );
    }
}

/// Forgets an agent's in-progress call list once it has gone silent, so the
/// UI never shows a call as live on a line nobody has heard from. The signal
/// reading is kept; the UI marks the line stale instead.
pub fn clear_active_call_details(agent: AgentKind, module_id: &str) {
    if let Some(view) = details()
        .lock()
        .unwrap()
        .get_mut(&(agent, module_id.to_string()))
    {
        if view.details.active_call_details.is_some() {
            view.details.active_call_details = Some(Vec::new());
        }
        // The count goes with the details: a count with no details would still
        // read as a call in progress.
        if view.active_calls.is_some() {
            view.active_calls = Some(0);
        }
    }
}

/// One agent's current state as the web UI needs it.
#[derive(Debug, Clone)]
pub struct AgentSnapshot {
    pub agent: AgentKind,
    pub module_id: String,
    /// Whether it reported within the staleness threshold.
    pub up: bool,
    pub age_seconds: f64,
    pub registered: Option<bool>,
    pub tunnel_up: Option<bool>,
    pub gm_connection_up: Option<bool>,
    pub child_sa_up: Option<bool>,
    pub pbx_registered: Option<bool>,
    pub active_calls: Option<u32>,
    pub uplink: Option<UplinkState>,
    pub flapping: bool,
    pub registration_expires_at: Option<u64>,
    pub details: AgentDetails,
}

/// Every agent that has reported since this process started.
pub fn snapshot_lines() -> Vec<AgentSnapshot> {
    snapshot_with(crate::metrics::server::staleness_threshold())
}

fn snapshot_with(staleness_threshold: Duration) -> Vec<AgentSnapshot> {
    // Copied out first so the two locks are never held together.
    let records: Vec<_> = liveness()
        .lock()
        .unwrap()
        .iter()
        .map(|(k, r)| (k.clone(), *r))
        .collect();
    let views = details().lock().unwrap();
    records
        .into_iter()
        .map(|((agent, module_id), record)| {
            let view = views
                .get(&(agent, module_id.clone()))
                .cloned()
                .unwrap_or_default();
            let age = record.last_report.elapsed();
            let up = age <= staleness_threshold;
            let mut view = view;
            // Cleared here as well as on scrape (`metrics::server`), since the
            // web UI reads this without anyone scraping /metrics.
            if !up {
                view.details.active_call_details = Some(Vec::new());
                view.active_calls = view.active_calls.map(|_| 0);
            }
            AgentSnapshot {
                agent,
                module_id,
                up,
                age_seconds: age.as_secs_f64(),
                registered: view.registered,
                tunnel_up: view.tunnel_up,
                gm_connection_up: view.gm_connection_up,
                child_sa_up: view.child_sa_up,
                pbx_registered: view.pbx_registered,
                active_calls: view.active_calls,
                uplink: record.last_uplink,
                flapping: record.tunnel_flapping,
                registration_expires_at: record.registration_expires_at,
                details: view.details,
            }
        })
        .collect()
}

/// Opens or extends a line's pool-exhaustion episode for each
/// `PcscfPoolExhausted` recovery in a supervisor report (specs/097).
fn note_pool_exhausted_rebuilds(
    episode: &mut PoolExhaustion,
    module_id: &str,
    events: &[ObservedEvent],
) {
    for event in events {
        let ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::PcscfPoolExhausted,
            pcscfs,
            rebuilt_at_unix,
            ..
        } = event
        else {
            continue;
        };
        if episode.since.is_none() {
            episode.since = Some(Instant::now());
            episode.rebuilds = 0;
        }
        episode.rebuilds = episode.rebuilds.saturating_add(1);
        episode.rebuilt_at_unix = *rebuilt_at_unix;
        pool_exhausted_pcscfs()
            .lock()
            .unwrap()
            .insert(module_id.to_string(), pcscfs.clone());
    }
}

/// The text of a pool-exhaustion alert (specs/097 contracts/alerts.md).
fn pool_exhausted_description(
    kind: CriticalEventKind,
    unhealthy_sec: u64,
    pcscfs: &[String],
    rebuilds: u32,
) -> String {
    match kind {
        CriticalEventKind::Failure => {
            let addrs = if pcscfs.is_empty() {
                "addresses unknown".to_string()
            } else {
                pcscfs.join(", ")
            };
            let plural = if rebuilds == 1 { "" } else { "s" };
            format!(
                "VoWiFi line's P-CSCF pool exhausted for over {unhealthy_sec}s ({addrs}; {rebuilds} tunnel rebuild{plural}): the carrier has not handed out a working P-CSCF"
            )
        }
        CriticalEventKind::Recovered => {
            "VoWiFi line registered again after its P-CSCF pool was exhausted".to_string()
        }
    }
}

/// specs/087: widens the supervisor's "unhealthy" from `!child_sa_up` to
/// `!child_sa_up || tunnel_flapping`, on top of what `apply_state` already did
/// with `child_sa_up`.
///
/// `prev_since` is the streak as it stood before this report. A flapping tunnel
/// whose CHILD_SA is up has no outage of its own, so its streak is opened
/// *backdated by the threshold* and the existing folded decision alerts on this
/// very report; a streak already open (an outage that overlaps flapping, or the
/// flapping streak itself) is kept, so overlapping time shares one episode and
/// one alert. The streak closes only when the CHILD_SA is up and flapping has
/// unlatched.
fn fold_flapping_into_tunnel_streak(
    since: &mut Option<Instant>,
    prev_since: Option<Instant>,
    flapping: bool,
    state: &AgentState,
    threshold: Option<Duration>,
    opened_by_flapping: &mut bool,
) {
    if flapping {
        // The streak flapping keeps open: whatever `apply_state` left, else the
        // one that stood before this report (a CHILD_SA reported up must not
        // end an episode that flapping is still holding open).
        let now = Instant::now();
        let target = threshold.and_then(|t| now.checked_sub(t)).unwrap_or(now);
        match since.or(prev_since) {
            // Already past the threshold: leave it, whoever opened it.
            Some(s) if s <= target => *since = Some(s),
            // Younger than the threshold (an outage that began with the latest
            // rebuild, or none yet): flapping alone is enough to alert, so
            // bring it back to the threshold rather than wait it out again.
            _ => {
                *since = Some(target);
                *opened_by_flapping = true;
            }
        }
    } else if state.tunnel_flapping == Some(false)
        && state.child_sa_up.is_none()
        && *opened_by_flapping
    {
        // Unlatched on a report that says nothing about the CHILD_SA: the only
        // thing keeping this streak open was the flapping.
        *since = None;
    }
    if since.is_none() {
        *opened_by_flapping = false;
    } else if prev_since.is_none() && !flapping {
        // A fresh streak that flapping did not open.
        *opened_by_flapping = false;
    }
}

/// The text of a tunnel-failure `Failure` alert (specs/087, contracts/
/// observability.md). `cause` is omitted when none was ever reported. While the
/// underlay guard holds the line (`uplink` down) the text says so (specs/100
/// FR-023); otherwise it is unchanged.
fn tunnel_failure_description(
    subject: &str,
    unhealthy_sec: u64,
    flapping: bool,
    cause: Option<IkeSaLossCause>,
    uplink: Option<UplinkState>,
) -> String {
    let text = match (flapping, cause) {
        (false, None) => {
            format!("{subject} line's tunnel non-established for over {unhealthy_sec}s")
        }
        (false, Some(c)) => format!(
            "{subject} line's tunnel non-established for over {unhealthy_sec}s (last IKE_SA loss: {c})"
        ),
        (true, None) => format!(
            "{subject} line's tunnel rebuilt repeatedly (3+ times within {unhealthy_sec}s)"
        ),
        (true, Some(c)) => format!(
            "{subject} line's tunnel rebuilt repeatedly (3+ times within {unhealthy_sec}s; last IKE_SA loss: {c})"
        ),
    };
    match uplink {
        Some(UplinkState::NoRoute) => format!("{text}; uplink down (no route), recovery on hold"),
        Some(UplinkState::NoResponse) => {
            format!("{text}; uplink down (no response), recovery on hold")
        }
        Some(UplinkState::Up) | None => text,
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

/// Bookkeeping for the folded tunnel-failure decision, kept on the supervisor's
/// record (specs/086).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FailureInFlight {
    /// The effective unhealthy-since a `Failure` was decided against. Frozen at
    /// decision time so a change in which signal is oldest cannot orphan the
    /// delivery callback's generation match.
    generation: Option<Instant>,
    /// Both signals recovered while that `Failure` was still being sent. The
    /// callback then owes the operator an all-clear instead of `Alerted`.
    ended: bool,
}

/// specs/086: the tunnel-failure decision when the IMS agent's tunnel signal and
/// the supervisor's CHILD_SA signal are folded into one. The effective streak
/// starts at the older of the two unhealthy timers, so either signal alone
/// alerts and an outage seen by both alerts once; it ends only when both are
/// healthy again. `generation` freezes the effective streak start when a
/// `Failure` is decided, for `record_alert_outcome`'s staleness check.
fn decide_folded_tunnel(
    ims_since: Option<Instant>,
    supervisor_since: Option<Instant>,
    phase: &mut AlertPhase,
    fold: &mut FailureInFlight,
    threshold: Duration,
) -> Transition {
    let effective = match (ims_since, supervisor_since) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    decide_tracked(effective, phase, fold, threshold)
}

/// [`decide_transition`] plus the bookkeeping for a `Failure` still being sent:
/// freezes the decided generation, and marks it `ended` when the streak clears
/// while `Pending`. The send cannot be cancelled and may yet succeed, so its
/// callback owes the operator the all-clear (see `record_alert_outcome`).
fn decide_tracked(
    since: Option<Instant>,
    phase: &mut AlertPhase,
    in_flight: &mut FailureInFlight,
    threshold: Duration,
) -> Transition {
    let t = decide_transition(since, phase, threshold);
    match t {
        Transition::Event(CriticalEventKind::Failure) => {
            in_flight.generation = since;
            in_flight.ended = false;
        }
        Transition::Event(CriticalEventKind::Recovered) => *in_flight = FailureInFlight::default(),
        _ => {}
    }
    if since.is_none() && *phase == AlertPhase::Pending {
        in_flight.ended = true;
    }
    t
}

enum Transition {
    None,
    Suppressed,
    Event(CriticalEventKind),
}

/// Names the line in a tunnel-failure alert. `Ims` and `Supervisor` both speak
/// for the same VoWiFi line; every other kind keeps its own name.
fn tunnel_alert_subject(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Ims | AgentKind::Supervisor => "VoWiFi",
        other => other.as_str(),
    }
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
    let Some((cfg, client, event)) = build_alert(&key, category, kind) else {
        return;
    };

    tokio::spawn(async move {
        let outcome = crate::alerts::dispatch(&client, &cfg, event).await;
        if kind == CriticalEventKind::Failure {
            let success = matches!(outcome, AlertOutcome::Sent(_));
            // `generation` is always `Some` here — only `Failure`
            // transitions reach this branch, and `decide_transition` only
            // ever produces one from `Some(since)`.
            if let Some(generation) = generation {
                if record_alert_outcome(&key, category, generation, success) {
                    // The outage ended while this notice was in flight, so the
                    // operator is owed the all-clear. It is sent from here, with
                    // the phase still `Pending`, so a new outage cannot race
                    // its own Failure ahead of it and then be wiped by this
                    // older all-clear.
                    if let Some((cfg, client, event)) =
                        build_alert(&key, category, CriticalEventKind::Recovered)
                    {
                        crate::alerts::dispatch(&client, &cfg, event).await;
                    }
                    finish_late_all_clear(&key, category);
                }
            }
        }
    });
}

/// Builds the event for one transition, or `None` when alerting is not set up.
fn build_alert(
    key: &(AgentKind, String),
    category: AlertCategory,
    kind: CriticalEventKind,
) -> Option<(AlertsConfig, DiscordClient, CriticalEvent)> {
    let (Some(cfg), Some(client)) = (ALERTS_CONFIG.get(), ALERTS_CLIENT.get()) else {
        return None;
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
        // The supervisor and the IMS agent share one folded alert (specs/086),
        // so both read as the VoWiFi line rather than naming whichever kind
        // happened to own the decision.
        (AlertCategory::TunnelFailure, CriticalEventKind::Failure) => {
            // The IMS agent's record has no uplink, so a failure decided on it
            // reads the supervisor's, the one that holds the line.
            let (flapping, cause, uplink) = {
                let records = liveness().lock().unwrap();
                let own = records.get(key);
                let supervisor = records.get(&(AgentKind::Supervisor, key.1.clone()));
                (
                    own.is_some_and(|r| r.streak_opened_by_flapping),
                    own.and_then(|r| r.last_ike_sa_loss_cause),
                    own.and_then(|r| r.last_uplink)
                        .or_else(|| supervisor.and_then(|r| r.last_uplink)),
                )
            };
            tunnel_failure_description(
                tunnel_alert_subject(key.0),
                cfg.tunnel_failure_thresholds.unhealthy_sec,
                flapping,
                cause,
                uplink,
            )
        }
        (AlertCategory::TunnelFailure, CriticalEventKind::Recovered) => {
            format!(
                "{} line's tunnel re-established",
                tunnel_alert_subject(key.0)
            )
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
        (AlertCategory::PcscfPoolExhausted, kind) => {
            let rebuilds = liveness()
                .lock()
                .unwrap()
                .get(key)
                .map_or(0, |r| r.pool_exhausted.rebuilds);
            let pcscfs = pool_exhausted_pcscfs()
                .lock()
                .unwrap()
                .get(&key.1)
                .cloned()
                .unwrap_or_default();
            pool_exhausted_description(
                kind,
                cfg.pcscf_pool_exhausted_thresholds.unhealthy_sec,
                &pcscfs,
                rebuilds,
            )
        }
        _ => unreachable!(
            "only RegistrationLoss/TunnelFailure/GmConnectionLost/PcscfPoolExhausted transitions are produced here"
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
    Some((cfg, client, event))
}

/// Completes a late all-clear started by [`record_alert_outcome`]: frees the
/// phase so any outage that began meanwhile is decided fresh.
fn finish_late_all_clear(key: &(AgentKind, String), category: AlertCategory) {
    if category == AlertCategory::PcscfPoolExhausted {
        let mut guard = liveness().lock().unwrap();
        if let Some(e) = guard.get_mut(key).map(|r| &mut r.pool_exhausted) {
            if e.phase == AlertPhase::Pending && e.ended {
                e.phase = AlertPhase::Idle;
                e.generation = None;
                e.ended = false;
            }
        }
        return;
    }
    let mut guard = liveness().lock().unwrap();
    let Some(record) = guard.get_mut(key) else {
        return;
    };
    let (phase, in_flight) = match category {
        AlertCategory::TunnelFailure => (&mut record.tunnel_alert_phase, &mut record.tunnel_fold),
        AlertCategory::RegistrationLoss => (
            &mut record.registered_alert_phase,
            &mut record.registered_in_flight,
        ),
        AlertCategory::GmConnectionLost => (
            &mut record.gm_connection_alert_phase,
            &mut record.gm_connection_in_flight,
        ),
        _ => return,
    };
    if *phase == AlertPhase::Pending && in_flight.ended {
        *in_flight = FailureInFlight::default();
        *phase = AlertPhase::Idle;
    }
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
) -> bool {
    let mut guard = liveness().lock().unwrap();
    let Some(record) = guard.get_mut(key) else {
        return false;
    };
    if category == AlertCategory::PcscfPoolExhausted {
        return record_pool_exhausted_outcome(&mut record.pool_exhausted, generation, success);
    }
    let (phase, in_flight, generation_now) = match category {
        AlertCategory::RegistrationLoss => (
            &mut record.registered_alert_phase,
            &mut record.registered_in_flight,
            None,
        ),
        // Records that never had a folded decision fall back to their own
        // streak start.
        AlertCategory::TunnelFailure => (
            &mut record.tunnel_alert_phase,
            &mut record.tunnel_fold,
            record.tunnel_unhealthy_since,
        ),
        AlertCategory::GmConnectionLost => (
            &mut record.gm_connection_alert_phase,
            &mut record.gm_connection_in_flight,
            None,
        ),
        _ => return false,
    };
    if *phase != AlertPhase::Pending || in_flight.generation.or(generation_now) != Some(generation)
    {
        return false;
    }
    // The incident ended while this send was in flight: do not park in
    // `Alerted` (nothing would ever send the all-clear). Go back to `Idle` so
    // any new outage is decided fresh, and tell the caller to send the
    // all-clear if the failure notice actually landed.
    if in_flight.ended {
        if success {
            // Stay `Pending` until the all-clear has been sent
            // (`finish_late_all_clear`), so nothing can be decided in between.
            return true;
        }
        *in_flight = FailureInFlight::default();
        *phase = AlertPhase::Idle;
        return false;
    }
    *phase = if success {
        AlertPhase::Alerted
    } else {
        AlertPhase::Idle
    };
    false
}

/// [`record_alert_outcome`] for a pool-exhaustion `Failure`, matched on its
/// frozen generation rather than the live episode start, which the line
/// registering clears (Greptile PR #137). Returns true when the episode ended
/// while the notice was in flight and it landed: the caller then owes the
/// all-clear, and [`finish_late_all_clear`] frees the phase after sending it.
fn record_pool_exhausted_outcome(
    e: &mut PoolExhaustion,
    generation: Instant,
    success: bool,
) -> bool {
    if e.phase != AlertPhase::Pending || e.generation != Some(generation) {
        return false;
    }
    if e.ended {
        if success {
            return true;
        }
        // Never delivered, so nothing to clear: decide afresh.
        e.phase = AlertPhase::Idle;
        e.generation = None;
        e.ended = false;
        return false;
    }
    e.phase = if success {
        AlertPhase::Alerted
    } else {
        AlertPhase::Idle
    };
    false
}

fn apply_state(
    agent: AgentKind,
    module_id: &str,
    state: &AgentState,
    registered_unhealthy_since: &mut Option<Instant>,
    tunnel_unhealthy_since: &mut Option<Instant>,
    gm_connection_unhealthy_since: &mut Option<Instant>,
    registration_expires_at: &mut Option<u64>,
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
            AgentKind::VolteSip | AgentKind::Supervisor => {}
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
            AgentKind::VolteSip | AgentKind::Supervisor => {}
        }
        if tunnel_up {
            *tunnel_unhealthy_since = None;
        } else if tunnel_unhealthy_since.is_none() {
            *tunnel_unhealthy_since = Some(Instant::now());
        }
    }
    // specs/086: the supervisor's CHILD_SA signal. It drives the supervisor
    // record's own unhealthy timer, which `apply_report` folds with the IMS
    // agent's tunnel timer so one outage alerts once.
    if let (AgentKind::Supervisor, Some(child_sa_up)) = (agent, state.child_sa_up) {
        metrics::VOWIFI_CHILD_SA_UP
            .with_label_values(&[module_id])
            .set(if child_sa_up { 1.0 } else { 0.0 });
        if child_sa_up {
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
            AgentKind::VolteSip | AgentKind::Supervisor => {}
        }
        if gm_connection_up {
            *gm_connection_unhealthy_since = None;
        } else if gm_connection_unhealthy_since.is_none() {
            *gm_connection_unhealthy_since = Some(Instant::now());
        }
    }
    // specs/039-at-stall-watchdog. Absent means "not reported", which must
    // leave the recorded expiry alone rather than clearing it -- clearing it
    // would silently retire the one signal that says the binding has lapsed.
    if let Some(expires_at) = state.registration_expires_at {
        *registration_expires_at = Some(expires_at);
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
        ObservedEvent::SmsForwarded {
            outcome,
            module: row_module,
            transport: row_transport,
        } => {
            let outcome_str = match outcome {
                SmsOutcome::Sent => "sent",
                SmsOutcome::Failed => "failed",
            };
            metrics::SMS_FORWARDED_TOTAL
                .with_label_values(&[
                    row_module.as_deref().unwrap_or(module_id),
                    outcome_str,
                    row_transport.as_deref().unwrap_or(transport),
                ])
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
        ObservedEvent::MediaQuality {
            source,
            loss_percent,
            jitter_seconds,
            round_trip_seconds,
        } => {
            let source_str = match source {
                QualitySource::Local => "local",
                QualitySource::Remote => "remote",
            };
            metrics::RTP_LOSS_PERCENT
                .with_label_values(&[module_id, source_str])
                .observe(*loss_percent);
            metrics::RTP_JITTER_SECONDS
                .with_label_values(&[module_id, source_str])
                .observe(*jitter_seconds);
            if let Some(rtt) = round_trip_seconds {
                metrics::RTP_ROUND_TRIP_SECONDS
                    .with_label_values(&[module_id])
                    .observe(*rtt);
            }
        }
        ObservedEvent::RtcpUnavailable => {
            metrics::RTCP_UNAVAILABLE_TOTAL
                .with_label_values(&[module_id])
                .inc();
        }
        ObservedEvent::UplinkHoldStarted { cause } => {
            metrics::VOWIFI_UPLINK_HOLDS_TOTAL
                .with_label_values(&[module_id, cause.as_str()])
                .inc();
            events::record(
                EventKind::UplinkHoldStarted,
                Some(module_id),
                &format!("cause: {}", cause.as_str()),
                None,
            );
        }
        ObservedEvent::UplinkHoldEnded {
            cause,
            held_secs,
            ended_by,
        } => {
            metrics::VOWIFI_UPLINK_HELD_SECONDS_TOTAL
                .with_label_values(&[module_id, cause.as_str()])
                .inc_by(*held_secs as f64);
            events::record(
                EventKind::UplinkHoldEnded,
                Some(module_id),
                &format!("cause: {}, ended by: {}", cause.as_str(), ended_by.as_str()),
                Some(*held_secs as f64),
            );
        }
        ObservedEvent::TunnelRecovered {
            reason,
            dead_peer,
            pcscfs,
            ..
        } => {
            metrics::VOWIFI_TUNNEL_RECOVERIES_TOTAL
                .with_label_values(&[module_id, reason.as_str()])
                .inc();
            let mut detail = format!("reason: {}", reason.as_str());
            if !pcscfs.is_empty() {
                detail.push_str(&format!(", P-CSCFs: {}", pcscfs.join(", ")));
            }
            events::record(EventKind::TunnelRecovered, Some(module_id), &detail, None);
            if let Some(verdict) = dead_peer {
                metrics::VOWIFI_DEAD_PEER_LOSSES_TOTAL
                    .with_label_values(&[module_id, verdict.as_str()])
                    .inc();
                if *verdict == crate::control::protocol::DeadPeerVerdict::Real {
                    events::record(
                        EventKind::DeadPeerLoss,
                        Some(module_id),
                        "the ePDG stopped answering IKE while the path was up",
                        None,
                    );
                }
            }
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
    /// When this line's registration lapses, if the agent reports it
    /// (specs/039-at-stall-watchdog). Absolute unix seconds; the scrape
    /// converts it to a countdown.
    pub registration_expires_at: Option<u64>,
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
                registration_expires_at: record.registration_expires_at,
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
            details: None,
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
            details: None,
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
            details: None,
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
            details: None,
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
            details: None,
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
            details: None,
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
            details: None,
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
            details: None,
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
                details: None,
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
            details: None,
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
            record.registered_in_flight.generation = Some(new_generation);
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
                registered_in_flight: FailureInFlight {
                    generation: Some(generation),
                    ended: false,
                },
                tunnel_unhealthy_since: None,
                tunnel_alert_phase: AlertPhase::Idle,
                tunnel_fold: FailureInFlight::default(),
                tunnel_flapping: false,
                last_ike_sa_loss_cause: None,
                last_uplink: None,
                streak_opened_by_flapping: false,
                gm_connection_unhealthy_since: None,
                gm_connection_alert_phase: AlertPhase::Idle,
                gm_connection_in_flight: FailureInFlight::default(),
                registration_expires_at: None,
                pool_exhausted: PoolExhaustion::default(),
                epoch_first_seen_unix: None,
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
            details: None,
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

    fn child_sa_report(agent: AgentKind, module_id: &str, seq: u64, up: Option<bool>) {
        apply_report(&AgentReport {
            agent,
            module_id: module_id.to_string(),
            epoch: 1,
            seq,
            state: AgentState {
                child_sa_up: up,
                ..AgentState::default()
            },
            events: vec![],
            dropped: 0,
            details: None,
        });
    }

    #[test]
    fn supervisor_child_sa_up_drives_the_gauge() {
        let m = "test-child-sa-gauge";
        child_sa_report(AgentKind::Supervisor, m, 1, Some(false));
        assert_eq!(
            metrics::VOWIFI_CHILD_SA_UP.with_label_values(&[m]).get(),
            0.0
        );
        child_sa_report(AgentKind::Supervisor, m, 2, Some(true));
        assert_eq!(
            metrics::VOWIFI_CHILD_SA_UP.with_label_values(&[m]).get(),
            1.0
        );
        // Absent leaves it alone.
        child_sa_report(AgentKind::Supervisor, m, 3, None);
        assert_eq!(
            metrics::VOWIFI_CHILD_SA_UP.with_label_values(&[m]).get(),
            1.0
        );
    }

    #[test]
    fn an_ims_report_never_touches_the_child_sa_gauge() {
        let m = "test-child-sa-ims-isolated";
        metrics::VOWIFI_CHILD_SA_UP.with_label_values(&[m]).set(1.0);
        child_sa_report(AgentKind::Ims, m, 1, Some(false));
        assert_eq!(
            metrics::VOWIFI_CHILD_SA_UP.with_label_values(&[m]).get(),
            1.0
        );
    }

    #[test]
    fn supervisor_unhealthy_timer_follows_child_sa_up() {
        let m = "test-child-sa-timer";
        child_sa_report(AgentKind::Supervisor, m, 1, Some(false));
        let key = (AgentKind::Supervisor, m.to_string());
        assert!(liveness().lock().unwrap()[&key]
            .tunnel_unhealthy_since
            .is_some());
        child_sa_report(AgentKind::Supervisor, m, 2, Some(true));
        assert!(liveness().lock().unwrap()[&key]
            .tunnel_unhealthy_since
            .is_none());
    }

    fn ago(secs: u64) -> Option<Instant> {
        Some(Instant::now() - Duration::from_secs(secs))
    }

    const FOLD_THRESHOLD: Duration = Duration::from_secs(300);

    fn fold(
        ims: Option<Instant>,
        sup: Option<Instant>,
        phase: &mut AlertPhase,
    ) -> (Transition, Option<Instant>) {
        let mut fold = FailureInFlight::default();
        let t = decide_folded_tunnel(ims, sup, phase, &mut fold, FOLD_THRESHOLD);
        (t, fold.generation)
    }

    #[test]
    fn a_missing_child_sa_alone_alerts_once() {
        let mut phase = AlertPhase::Idle;
        let (t, generation) = fold(None, ago(400), &mut phase);
        assert!(matches!(t, Transition::Event(CriticalEventKind::Failure)));
        assert!(generation.is_some());
        assert_eq!(phase, AlertPhase::Pending);
    }

    #[test]
    fn a_dead_ims_tunnel_alone_alerts_once() {
        let mut phase = AlertPhase::Idle;
        let (t, _) = fold(ago(400), None, &mut phase);
        assert!(matches!(t, Transition::Event(CriticalEventKind::Failure)));
    }

    #[test]
    fn both_signals_unhealthy_alert_exactly_once() {
        let mut phase = AlertPhase::Idle;
        let ims = ago(400);
        let sup = ago(500);
        let (t, generation) = fold(ims, sup, &mut phase);
        assert!(matches!(t, Transition::Event(CriticalEventKind::Failure)));
        assert_eq!(generation, sup, "the older streak start is the generation");
        // Further reports from either side, same outage: nothing more.
        let (t2, _) = fold(ims, sup, &mut phase);
        assert!(matches!(t2, Transition::None));
        phase = AlertPhase::Alerted;
        let (t3, _) = fold(ims, sup, &mut phase);
        assert!(matches!(t3, Transition::Suppressed));
    }

    #[test]
    fn recovery_waits_for_both_signals_then_fires_once() {
        let mut phase = AlertPhase::Alerted;
        // One recovered, the other still down: no Recovered.
        let (t, _) = fold(None, ago(900), &mut phase);
        assert!(!matches!(t, Transition::Event(_)));
        assert_eq!(phase, AlertPhase::Alerted);
        // Both healthy: exactly one Recovered.
        let (t, _) = fold(None, None, &mut phase);
        assert!(matches!(t, Transition::Event(CriticalEventKind::Recovered)));
        let (t, _) = fold(None, None, &mut phase);
        assert!(matches!(t, Transition::None));
    }

    #[test]
    fn a_folded_failure_resolves_against_the_supervisors_frozen_generation() {
        let m = "test-fold-outcome";
        let key = (AgentKind::Supervisor, m.to_string());
        let generation = Instant::now();
        {
            let mut guard = liveness().lock().unwrap();
            guard.insert(
                key.clone(),
                AgentRecord {
                    last_report: Instant::now(),
                    last_applied: (0, 0),
                    registered_unhealthy_since: None,
                    registered_alert_phase: AlertPhase::Idle,
                    registered_in_flight: FailureInFlight::default(),
                    // The streak start has since moved on; the frozen generation must win.
                    tunnel_unhealthy_since: Some(Instant::now()),
                    tunnel_alert_phase: AlertPhase::Pending,
                    tunnel_fold: FailureInFlight {
                        generation: Some(generation),
                        ended: false,
                    },
                    tunnel_flapping: false,
                    last_ike_sa_loss_cause: None,
                    last_uplink: None,
                    streak_opened_by_flapping: false,
                    gm_connection_unhealthy_since: None,
                    gm_connection_alert_phase: AlertPhase::Idle,
                    gm_connection_in_flight: FailureInFlight::default(),
                    registration_expires_at: None,
                    pool_exhausted: PoolExhaustion::default(),
                    epoch_first_seen_unix: None,
                },
            );
        }
        // A stale generation is a no-op.
        record_alert_outcome(
            &key,
            AlertCategory::TunnelFailure,
            generation - Duration::from_secs(1),
            true,
        );
        assert_eq!(
            liveness().lock().unwrap()[&key].tunnel_alert_phase,
            AlertPhase::Pending
        );
        record_alert_outcome(&key, AlertCategory::TunnelFailure, generation, true);
        assert_eq!(
            liveness().lock().unwrap()[&key].tunnel_alert_phase,
            AlertPhase::Alerted
        );
    }

    #[test]
    fn tunnel_alert_text_names_the_vowifi_line_for_both_kinds_only() {
        assert_eq!(tunnel_alert_subject(AgentKind::Ims), "VoWiFi");
        assert_eq!(tunnel_alert_subject(AgentKind::Supervisor), "VoWiFi");
        assert_eq!(tunnel_alert_subject(AgentKind::Volte), "volte");
    }

    // --- specs/097: the pool-exhaustion episode -----------------------------

    /// A rebuild the supervisor issued `ago` seconds before now.
    fn exhausted(addr: &str, ago: u64) -> ObservedEvent {
        ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::PcscfPoolExhausted,
            pcscfs: vec![addr.to_string()],
            rebuilt_at_unix: Some(now_unix() - ago),
            dead_peer: None,
        }
    }

    fn sup_report(
        cfg: &AlertsConfig,
        m: &str,
        seq: u64,
        events: Vec<ObservedEvent>,
    ) -> Vec<PendingTransition> {
        apply_report_with(
            &AgentReport {
                agent: AgentKind::Supervisor,
                module_id: m.to_string(),
                epoch: 1,
                seq,
                state: AgentState {
                    child_sa_up: Some(true),
                    tunnel_flapping: Some(false),
                    ..AgentState::default()
                },
                events,
                dropped: 0,
                details: None,
            },
            Some(cfg),
        )
    }

    fn ims_report(cfg: &AlertsConfig, m: &str, epoch: u64, seq: u64, registered: bool) {
        apply_report_with(
            &AgentReport {
                agent: AgentKind::Ims,
                module_id: m.to_string(),
                epoch,
                seq,
                state: AgentState {
                    registered: Some(registered),
                    ..AgentState::default()
                },
                events: vec![],
                dropped: 0,
                details: None,
            },
            Some(cfg),
        );
    }

    /// Makes the line's current IMS agent look `secs` older to the daemon.
    fn age_ims_agent(m: &str, secs: u64) {
        let mut guard = liveness().lock().unwrap();
        let r = guard.get_mut(&(AgentKind::Ims, m.to_string())).unwrap();
        r.epoch_first_seen_unix = r.epoch_first_seen_unix.map(|t| t - secs);
    }

    fn pool_events(t: &[PendingTransition]) -> Vec<CriticalEventKind> {
        t.iter()
            .filter(|(_, c, _, _)| *c == AlertCategory::PcscfPoolExhausted)
            .map(|(_, _, kind, _)| *kind)
            .collect()
    }

    fn episode(m: &str) -> PoolExhaustion {
        liveness().lock().unwrap()[&(AgentKind::Supervisor, m.to_string())].pool_exhausted
    }

    fn backdate_episode(m: &str, secs: u64) {
        let mut guard = liveness().lock().unwrap();
        let r = guard
            .get_mut(&(AgentKind::Supervisor, m.to_string()))
            .unwrap();
        r.pool_exhausted.since = Some(Instant::now() - Duration::from_secs(secs));
    }

    /// Opens an episode on `m` and takes it past the threshold, returning the
    /// `Failure`'s generation with the send still in flight (`Pending`).
    fn episode_alerting(cfg: &AlertsConfig, m: &str) -> Instant {
        ims_report(cfg, m, 100, 1, true);
        age_ims_agent(m, 1000);
        sup_report(cfg, m, 1, vec![exhausted("10.0.0.11", 500)]);
        backdate_episode(m, 301);
        let t = sup_report(cfg, m, 2, vec![]);
        assert_eq!(pool_events(&t), vec![CriticalEventKind::Failure]);
        t[0].3.unwrap()
    }

    #[test]
    fn a_long_exhaustion_alerts_once_and_all_clears_once_the_line_registers() {
        let m = "test-pool-exhausted-episode";
        let cfg = AlertsConfig::default();
        let key = (AgentKind::Supervisor, m.to_string());
        let generation = episode_alerting(&cfg, m);
        assert_eq!(episode(m).rebuilds, 1);
        assert!(!record_alert_outcome(
            &key,
            AlertCategory::PcscfPoolExhausted,
            generation,
            true
        ));
        assert_eq!(episode(m).phase, AlertPhase::Alerted);

        // More rebuilds in the same episode count, but never alert again.
        for seq in 3..6 {
            let t = sup_report(&cfg, m, seq, vec![exhausted("10.0.0.11", 400)]);
            assert!(pool_events(&t).is_empty());
        }
        assert_eq!(episode(m).rebuilds, 4);

        // The agent being replaced still heartbeats `registered`: not the end.
        ims_report(&cfg, m, 100, 2, true);
        assert!(episode(m).since.is_some());
        // An agent first seen after the latest rebuild registering is.
        ims_report(&cfg, m, 300, 1, true);
        assert_eq!(episode(m).since, None);
        assert_eq!(
            pool_events(&sup_report(&cfg, m, 6, vec![])),
            vec![CriticalEventKind::Recovered]
        );
        assert!(pool_events(&sup_report(&cfg, m, 7, vec![])).is_empty());
    }

    #[test]
    fn a_rebuild_report_that_arrives_after_the_replacement_registered_still_ends() {
        // Greptile PR #137: both reporters queue behind an unreachable daemon
        // and flush in either order. The replacement's registration lands
        // first; the rebuild report, carrying its own time, lands after.
        let m = "test-pool-exhausted-late-report";
        let cfg = AlertsConfig::default();
        ims_report(&cfg, m, 100, 1, true);
        age_ims_agent(m, 1000);
        ims_report(&cfg, m, 200, 1, true); // the replacement, already up
        sup_report(&cfg, m, 1, vec![exhausted("10.0.0.11", 5)]);
        assert!(episode(m).since.is_some(), "the rebuild did happen");
        // Its next heartbeat is from an agent first seen after the rebuild.
        ims_report(&cfg, m, 200, 2, true);
        assert_eq!(episode(m).since, None);
    }

    #[test]
    fn an_episode_that_ends_while_its_alert_is_in_flight_still_all_clears() {
        // Greptile PR #137: the line registering mid-send used to leave the
        // phase `Pending` forever, silencing every later alert for the line.
        let m = "test-pool-exhausted-ended-in-flight";
        let cfg = AlertsConfig::default();
        let key = (AgentKind::Supervisor, m.to_string());
        let generation = episode_alerting(&cfg, m);
        ims_report(&cfg, m, 300, 1, true);
        assert_eq!(episode(m).since, None);
        assert!(episode(m).ended);
        // The notice landed: the all-clear is owed, then the phase is freed.
        assert!(record_alert_outcome(
            &key,
            AlertCategory::PcscfPoolExhausted,
            generation,
            true
        ));
        finish_late_all_clear(&key, AlertCategory::PcscfPoolExhausted);
        assert_eq!(episode(m).phase, AlertPhase::Idle);
        // And a later episode alerts again.
        sup_report(&cfg, m, 3, vec![exhausted("10.0.0.11", 1)]);
        backdate_episode(m, 301);
        assert_eq!(
            pool_events(&sup_report(&cfg, m, 4, vec![])),
            vec![CriticalEventKind::Failure]
        );
    }

    #[test]
    fn an_undelivered_alert_for_an_ended_episode_is_simply_dropped() {
        let m = "test-pool-exhausted-ended-undelivered";
        let cfg = AlertsConfig::default();
        let key = (AgentKind::Supervisor, m.to_string());
        let generation = episode_alerting(&cfg, m);
        ims_report(&cfg, m, 300, 1, true);
        assert!(!record_alert_outcome(
            &key,
            AlertCategory::PcscfPoolExhausted,
            generation,
            false
        ));
        let e = episode(m);
        assert_eq!(
            (e.phase, e.ended, e.generation),
            (AlertPhase::Idle, false, None)
        );
        assert!(pool_events(&sup_report(&cfg, m, 3, vec![])).is_empty());
    }

    #[test]
    fn a_short_exhaustion_never_alerts() {
        let m = "test-pool-exhausted-short";
        let cfg = AlertsConfig::default();
        ims_report(&cfg, m, 100, 1, false);
        sup_report(&cfg, m, 1, vec![exhausted("10.0.0.11", 10)]);
        ims_report(&cfg, m, 101, 1, true);
        assert_eq!(episode(m).since, None);
        for seq in 2..4 {
            assert!(pool_events(&sup_report(&cfg, m, seq, vec![])).is_empty());
        }
    }

    #[test]
    fn a_line_that_never_reported_an_agent_ends_on_its_first_registration() {
        let m = "test-pool-exhausted-no-agent";
        let cfg = AlertsConfig::default();
        sup_report(&cfg, m, 1, vec![exhausted("2001:db8::1", 10)]);
        assert!(episode(m).since.is_some());
        ims_report(&cfg, m, 5, 1, true);
        assert_eq!(episode(m).since, None);
    }

    #[test]
    fn other_recovery_reasons_never_open_an_episode() {
        let m = "test-pool-exhausted-other-reasons";
        let cfg = AlertsConfig::default();
        let events = TunnelRecoveryReason::ALL
            .into_iter()
            .filter(|r| *r != TunnelRecoveryReason::PcscfPoolExhausted)
            .map(|reason| ObservedEvent::TunnelRecovered {
                reason,
                pcscfs: Vec::new(),
                rebuilt_at_unix: None,
                dead_peer: None,
            })
            .collect();
        sup_report(&cfg, m, 1, events);
        assert_eq!(episode(m), PoolExhaustion::default());
    }

    #[test]
    fn a_disabled_or_unconfigured_category_is_skipped_at_dispatch() {
        // `enabled` and the webhook are honoured in `alerts::dispatch`, as for
        // every category; the decision above is the same either way.
        use crate::config::secret::Secret;
        let mut cfg = AlertsConfig::default();
        assert!(cfg.pcscf_pool_exhausted.enabled, "on by default");
        assert_eq!(
            crate::alerts::precheck(&cfg.pcscf_pool_exhausted, &cfg.default_webhook_url),
            Some(AlertOutcome::Skipped),
            "no webhook anywhere"
        );
        cfg.default_webhook_url = Secret::new("https://discord.com/api/webhooks/x".into());
        assert_eq!(
            crate::alerts::precheck(&cfg.pcscf_pool_exhausted, &cfg.default_webhook_url),
            None
        );
        cfg.pcscf_pool_exhausted.enabled = false;
        assert_eq!(
            crate::alerts::precheck(&cfg.pcscf_pool_exhausted, &cfg.default_webhook_url),
            Some(AlertOutcome::Skipped)
        );
    }

    #[test]
    fn pool_exhausted_alert_text_names_the_addresses_and_the_rebuilds() {
        let failure = pool_exhausted_description(
            CriticalEventKind::Failure,
            300,
            &["10.0.0.11".to_string()],
            4,
        );
        assert_eq!(
            failure,
            "VoWiFi line's P-CSCF pool exhausted for over 300s (10.0.0.11; 4 tunnel rebuilds): the carrier has not handed out a working P-CSCF"
        );
        assert!(
            pool_exhausted_description(CriticalEventKind::Failure, 300, &[], 1)
                .contains("(addresses unknown; 1 tunnel rebuild)")
        );
        assert_eq!(
            pool_exhausted_description(CriticalEventKind::Recovered, 300, &[], 4),
            "VoWiFi line registered again after its P-CSCF pool was exhausted"
        );
    }

    // --- Two-reporter wiring through `apply_report_with` (specs/086) ---------

    fn tunnel_cfg() -> AlertsConfig {
        let mut cfg = AlertsConfig::default();
        cfg.tunnel_failure_thresholds.unhealthy_sec = 300;
        cfg
    }

    fn fold_report(
        cfg: &AlertsConfig,
        agent: AgentKind,
        module_id: &str,
        seq: u64,
        up: bool,
    ) -> Vec<PendingTransition> {
        let state = match agent {
            AgentKind::Supervisor => AgentState {
                child_sa_up: Some(up),
                ..AgentState::default()
            },
            _ => AgentState {
                tunnel_up: Some(up),
                ..AgentState::default()
            },
        };
        apply_report_with(
            &AgentReport {
                agent,
                module_id: module_id.to_string(),
                epoch: 7,
                seq,
                state,
                events: vec![],
                dropped: 0,
                details: None,
            },
            Some(cfg),
        )
    }

    fn tunnel_events(t: &[PendingTransition]) -> Vec<(AgentKind, CriticalEventKind)> {
        t.iter()
            .filter(|(_, c, _, _)| *c == AlertCategory::TunnelFailure)
            .map(|(k, _, kind, _)| (k.0, *kind))
            .collect()
    }

    fn backdate_tunnel(agent: AgentKind, module_id: &str, secs: u64) {
        let mut guard = liveness().lock().unwrap();
        let r = guard.get_mut(&(agent, module_id.to_string())).unwrap();
        r.tunnel_unhealthy_since = r
            .tunnel_unhealthy_since
            .map(|_| Instant::now() - Duration::from_secs(secs));
    }

    #[test]
    fn interleaved_reporters_alert_once_per_outage_and_recover_when_both_are_healthy() {
        let m = "test-two-reporter-outage";
        let cfg = tunnel_cfg();
        let sup_key = (AgentKind::Supervisor, m.to_string());

        // Both healthy, then both go down; neither is past the threshold yet.
        assert!(fold_report(&cfg, AgentKind::Supervisor, m, 1, true).is_empty());
        assert!(fold_report(&cfg, AgentKind::Ims, m, 1, true).is_empty());
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Supervisor, m, 2, false)).is_empty());
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Ims, m, 2, false)).is_empty());

        // The supervisor's streak crosses the threshold: one Failure, owned by
        // the supervisor's record.
        backdate_tunnel(AgentKind::Supervisor, m, 400);
        backdate_tunnel(AgentKind::Ims, m, 350);
        let t = fold_report(&cfg, AgentKind::Supervisor, m, 3, false);
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Supervisor, CriticalEventKind::Failure)]
        );
        let generation = t[0].3.expect("failure carries its generation");
        // The IMS agent's own report must not produce a second Failure.
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Ims, m, 3, false)).is_empty());
        assert_eq!(
            liveness().lock().unwrap()[&(AgentKind::Ims, m.to_string())].tunnel_alert_phase,
            AlertPhase::Idle,
            "the IMS record never owns the folded decision"
        );

        // Delivery result lands on the supervisor's record.
        record_alert_outcome(&sup_key, AlertCategory::TunnelFailure, generation, true);
        assert_eq!(
            liveness().lock().unwrap()[&sup_key].tunnel_alert_phase,
            AlertPhase::Alerted
        );

        // Partial recovery: the supervisor is healthy, the IMS agent is not.
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Supervisor, m, 4, true)).is_empty());
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Ims, m, 4, false)).is_empty());
        // Full recovery: exactly one Recovered.
        let t = fold_report(&cfg, AgentKind::Ims, m, 5, true);
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Supervisor, CriticalEventKind::Recovered)]
        );
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Supervisor, m, 5, true)).is_empty());
    }

    #[test]
    fn a_late_delivery_after_recovery_sends_the_all_clear_and_frees_the_next_outage() {
        let m = "test-two-reporter-late-delivery";
        let cfg = tunnel_cfg();
        let sup_key = (AgentKind::Supervisor, m.to_string());

        fold_report(&cfg, AgentKind::Supervisor, m, 1, false);
        backdate_tunnel(AgentKind::Supervisor, m, 400);
        let t = fold_report(&cfg, AgentKind::Supervisor, m, 2, false);
        let generation = t[0].3.unwrap();

        // Everything recovers while the send is still in flight, and a fresh
        // outage begins before it resolves. The new outage must wait.
        fold_report(&cfg, AgentKind::Supervisor, m, 3, true);
        fold_report(&cfg, AgentKind::Supervisor, m, 4, false);
        backdate_tunnel(AgentKind::Supervisor, m, 400);
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Supervisor, m, 5, false)).is_empty());

        // The first send lands: the operator is owed an all-clear for it, and
        // the phase is freed rather than parked in Alerted.
        assert!(record_alert_outcome(
            &sup_key,
            AlertCategory::TunnelFailure,
            generation,
            true
        ));
        // The phase is held until the all-clear has gone out, so the ongoing
        // second outage cannot send a Failure that the older all-clear would
        // then wipe.
        assert_eq!(
            liveness().lock().unwrap()[&sup_key].tunnel_alert_phase,
            AlertPhase::Pending
        );
        assert!(tunnel_events(&fold_report(&cfg, AgentKind::Supervisor, m, 6, false)).is_empty());
        finish_late_all_clear(&sup_key, AlertCategory::TunnelFailure);
        assert_eq!(
            liveness().lock().unwrap()[&sup_key].tunnel_alert_phase,
            AlertPhase::Idle
        );

        // Only now does the ongoing second outage get its own Failure.
        let t = fold_report(&cfg, AgentKind::Supervisor, m, 7, false);
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Supervisor, CriticalEventKind::Failure)]
        );
    }

    #[test]
    fn a_failed_delivery_after_recovery_sends_no_all_clear() {
        let m = "test-two-reporter-late-failed-delivery";
        let cfg = tunnel_cfg();
        let sup_key = (AgentKind::Supervisor, m.to_string());
        fold_report(&cfg, AgentKind::Supervisor, m, 1, false);
        backdate_tunnel(AgentKind::Supervisor, m, 400);
        let generation = fold_report(&cfg, AgentKind::Supervisor, m, 2, false)[0]
            .3
            .unwrap();
        fold_report(&cfg, AgentKind::Supervisor, m, 3, true);
        assert!(!record_alert_outcome(
            &sup_key,
            AlertCategory::TunnelFailure,
            generation,
            false
        ));
        assert_eq!(
            liveness().lock().unwrap()[&sup_key].tunnel_alert_phase,
            AlertPhase::Idle
        );
    }

    #[test]
    fn a_supervisor_free_module_keeps_its_own_per_record_decision() {
        let m = "test-two-reporter-no-supervisor";
        let cfg = tunnel_cfg();
        fold_report(&cfg, AgentKind::Ims, m, 1, false);
        backdate_tunnel(AgentKind::Ims, m, 400);
        let t = fold_report(&cfg, AgentKind::Ims, m, 2, false);
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Ims, CriticalEventKind::Failure)]
        );
    }

    // specs/087: flapping widens the supervisor's "unhealthy".

    fn sup_state_report(
        cfg: &AlertsConfig,
        module_id: &str,
        seq: u64,
        state: AgentState,
    ) -> Vec<PendingTransition> {
        apply_report_with(
            &AgentReport {
                agent: AgentKind::Supervisor,
                module_id: module_id.to_string(),
                epoch: 7,
                seq,
                state,
                events: vec![],
                dropped: 0,
                details: None,
            },
            Some(cfg),
        )
    }

    fn flap_state(child_sa_up: bool, flapping: bool) -> AgentState {
        AgentState {
            child_sa_up: Some(child_sa_up),
            tunnel_flapping: Some(flapping),
            ..AgentState::default()
        }
    }

    #[test]
    fn a_flapping_tunnel_alerts_once_while_its_child_sa_is_up_and_recovers_when_it_settles() {
        let m = "test-flapping-alert";
        let cfg = tunnel_cfg();
        let sup_key = (AgentKind::Supervisor, m.to_string());
        assert!(tunnel_events(&sup_state_report(&cfg, m, 1, flap_state(true, false))).is_empty());
        // The third rebuild latches: one Failure, on that same report.
        let t = sup_state_report(&cfg, m, 2, flap_state(true, true));
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Supervisor, CriticalEventKind::Failure)]
        );
        assert!(
            liveness().lock().unwrap()[&sup_key].streak_opened_by_flapping,
            "the alert text must say rebuilt-repeatedly"
        );
        // Still flapping: nothing more.
        assert!(tunnel_events(&sup_state_report(&cfg, m, 3, flap_state(true, true))).is_empty());
        // Settled: the failure notice lands, then the all-clear.
        liveness()
            .lock()
            .unwrap()
            .get_mut(&sup_key)
            .unwrap()
            .tunnel_alert_phase = AlertPhase::Alerted;
        let t = sup_state_report(&cfg, m, 4, flap_state(true, false));
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Supervisor, CriticalEventKind::Recovered)]
        );
        assert!(!liveness().lock().unwrap()[&sup_key].streak_opened_by_flapping);
    }

    #[test]
    fn an_outage_overlapping_flapping_alerts_exactly_once() {
        let m = "test-flapping-overlaps-outage";
        let cfg = tunnel_cfg();
        let sup_key = (AgentKind::Supervisor, m.to_string());
        let mut failures = 0;
        let mut seq = 0;
        let mut go = |state: AgentState| {
            seq += 1;
            failures += tunnel_events(&sup_state_report(&cfg, m, seq, state))
                .iter()
                .filter(|(_, k)| *k == CriticalEventKind::Failure)
                .count();
        };
        go(flap_state(true, true));
        liveness()
            .lock()
            .unwrap()
            .get_mut(&sup_key)
            .unwrap()
            .tunnel_alert_phase = AlertPhase::Alerted;
        // The CHILD_SA now drops too, past the threshold: still the one episode.
        go(flap_state(false, true));
        backdate_tunnel(AgentKind::Supervisor, m, 400);
        go(flap_state(false, true));
        assert_eq!(failures, 1);
    }

    #[test]
    fn an_ims_report_never_sets_or_clears_flapping() {
        let m = "test-flapping-ims-untouched";
        let cfg = tunnel_cfg();
        sup_state_report(&cfg, m, 1, flap_state(true, true));
        fold_report(&cfg, AgentKind::Ims, m, 1, true);
        let g = liveness().lock().unwrap();
        assert!(g[&(AgentKind::Supervisor, m.to_string())].tunnel_flapping);
        assert!(!g[&(AgentKind::Ims, m.to_string())].tunnel_flapping);
    }

    #[test]
    fn the_loss_cause_is_remembered_and_named_in_the_alert_text() {
        let m = "test-loss-cause-sticks";
        let cfg = tunnel_cfg();
        sup_state_report(
            &cfg,
            m,
            1,
            AgentState {
                child_sa_up: Some(false),
                ike_sa_loss_cause: Some(IkeSaLossCause::CarrierDelete),
                ..AgentState::default()
            },
        );
        // A later report that does not mention the cause leaves it alone.
        sup_state_report(&cfg, m, 2, flap_state(false, false));
        let cause = liveness().lock().unwrap()[&(AgentKind::Supervisor, m.to_string())]
            .last_ike_sa_loss_cause;
        assert_eq!(cause, Some(IkeSaLossCause::CarrierDelete));
    }

    #[test]
    fn tunnel_failure_text_per_cause_and_streak_kind() {
        use IkeSaLossCause::*;
        assert_eq!(
            tunnel_failure_description("VoWiFi", 300, false, None, None),
            "VoWiFi line's tunnel non-established for over 300s"
        );
        assert_eq!(
            tunnel_failure_description("VoWiFi", 300, false, Some(DeadPeer), None),
            "VoWiFi line's tunnel non-established for over 300s (last IKE_SA loss: dead-peer timeout)"
        );
        assert_eq!(
            tunnel_failure_description("VoWiFi", 300, true, Some(CarrierDelete), None),
            "VoWiFi line's tunnel rebuilt repeatedly (3+ times within 300s; last IKE_SA loss: carrier DELETE)"
        );
        let no_cause = tunnel_failure_description("VoWiFi", 300, true, None, None);
        assert!(no_cause.contains("rebuilt repeatedly") && !no_cause.contains("last IKE_SA loss"));
    }

    #[test]
    fn an_alert_for_a_held_line_says_the_uplink_is_down_and_nothing_else_changes() {
        use IkeSaLossCause::*;
        for flapping in [false, true] {
            for cause in [None, Some(DeadPeer), Some(CarrierDelete)] {
                let plain = tunnel_failure_description("VoWiFi", 300, flapping, cause, None);
                // Up or absent: byte for byte what it was.
                assert_eq!(
                    tunnel_failure_description(
                        "VoWiFi",
                        300,
                        flapping,
                        cause,
                        Some(UplinkState::Up)
                    ),
                    plain
                );
                assert_eq!(
                    tunnel_failure_description(
                        "VoWiFi",
                        300,
                        flapping,
                        cause,
                        Some(UplinkState::NoRoute)
                    ),
                    format!("{plain}; uplink down (no route), recovery on hold")
                );
                assert_eq!(
                    tunnel_failure_description(
                        "VoWiFi",
                        300,
                        flapping,
                        cause,
                        Some(UplinkState::NoResponse)
                    ),
                    format!("{plain}; uplink down (no response), recovery on hold")
                );
            }
        }
        assert_eq!(
            tunnel_failure_description(
                "VoWiFi",
                300,
                false,
                Some(DeadPeer),
                Some(UplinkState::NoRoute)
            ),
            "VoWiFi line's tunnel non-established for over 300s (last IKE_SA loss: dead-peer timeout); uplink down (no route), recovery on hold"
        );
    }

    fn uplink_report(m: &str, seq: u64, uplink: Option<UplinkState>, events: Vec<ObservedEvent>) {
        apply_report(&AgentReport {
            agent: AgentKind::Supervisor,
            module_id: m.to_string(),
            epoch: 1,
            seq,
            state: AgentState {
                uplink,
                ..AgentState::default()
            },
            events,
            dropped: 0,
            details: None,
        });
    }

    #[test]
    fn the_uplink_held_gauge_follows_the_supervisors_state_and_ignores_silence() {
        let m = "test-uplink-held-gauge";
        let gauge = || metrics::VOWIFI_UPLINK_HELD.with_label_values(&[m]).get();
        uplink_report(m, 1, Some(UplinkState::NoRoute), vec![]);
        assert_eq!(gauge(), 1.0);
        // A report that does not carry the signal leaves it as it was.
        uplink_report(m, 2, None, vec![]);
        assert_eq!(gauge(), 1.0);
        uplink_report(m, 3, Some(UplinkState::NoResponse), vec![]);
        assert_eq!(gauge(), 1.0);
        uplink_report(m, 4, Some(UplinkState::Up), vec![]);
        assert_eq!(gauge(), 0.0);
        let key = (AgentKind::Supervisor, m.to_string());
        assert_eq!(
            liveness().lock().unwrap()[&key].last_uplink,
            Some(UplinkState::Up)
        );
    }

    #[test]
    fn hold_events_count_starts_and_add_up_held_seconds_by_cause() {
        use crate::control::protocol::{HoldEndedBy, UplinkHoldCause};
        let m = "test-uplink-hold-events";
        let holds = |c: &str| {
            metrics::VOWIFI_UPLINK_HOLDS_TOTAL
                .with_label_values(&[m, c])
                .get()
        };
        let secs = |c: &str| {
            metrics::VOWIFI_UPLINK_HELD_SECONDS_TOTAL
                .with_label_values(&[m, c])
                .get()
        };
        uplink_report(
            m,
            1,
            Some(UplinkState::NoRoute),
            vec![ObservedEvent::UplinkHoldStarted {
                cause: UplinkHoldCause::NoRoute,
            }],
        );
        assert_eq!((holds("no_route"), holds("no_response")), (1.0, 0.0));
        assert_eq!(secs("no_route"), 0.0);
        uplink_report(
            m,
            2,
            Some(UplinkState::Up),
            vec![ObservedEvent::UplinkHoldEnded {
                cause: UplinkHoldCause::NoRoute,
                held_secs: 312,
                ended_by: HoldEndedBy::Restored,
            }],
        );
        assert_eq!(secs("no_route"), 312.0);
        assert_eq!(secs("no_response"), 0.0);
        uplink_report(
            m,
            3,
            None,
            vec![ObservedEvent::UplinkHoldEnded {
                cause: UplinkHoldCause::NoRoute,
                held_secs: 8,
                ended_by: HoldEndedBy::Shutdown,
            }],
        );
        assert_eq!(secs("no_route"), 320.0);
    }

    #[test]
    fn flapping_backdates_a_fresh_outage_timer_so_the_alert_is_not_delayed() {
        let m = "test-flapping-backdates-outage";
        let cfg = tunnel_cfg();
        // The third rebuild reports the CHILD_SA down: `apply_state` opens a
        // brand-new outage timer, which flapping must not leave waiting.
        let t = sup_state_report(&cfg, m, 1, flap_state(false, true));
        assert_eq!(
            tunnel_events(&t),
            vec![(AgentKind::Supervisor, CriticalEventKind::Failure)]
        );
        // The CHILD_SA coming back while still flapping keeps the same episode.
        assert!(tunnel_events(&sup_state_report(&cfg, m, 2, flap_state(true, true))).is_empty());
    }

    // --- #138: recovery while the Failure is still being sent ---------------

    fn zero_cfg() -> AlertsConfig {
        let mut cfg = AlertsConfig::default();
        cfg.registration_loss_thresholds.unhealthy_sec = 0;
        cfg.gm_connection_lost_thresholds.unhealthy_sec = 0;
        cfg
    }

    fn generic_report(
        cfg: &AlertsConfig,
        category: AlertCategory,
        m: &str,
        seq: u64,
        healthy: bool,
    ) -> Vec<CriticalEventKind> {
        let state = match category {
            AlertCategory::RegistrationLoss => AgentState {
                registered: Some(healthy),
                ..AgentState::default()
            },
            _ => AgentState {
                gm_connection_up: Some(healthy),
                ..AgentState::default()
            },
        };
        apply_report_with(
            &AgentReport {
                agent: AgentKind::Ims,
                module_id: m.to_string(),
                epoch: 3,
                seq,
                state,
                events: vec![],
                dropped: 0,
                details: None,
            },
            Some(cfg),
        )
        .into_iter()
        .filter(|(_, c, _, _)| *c == category)
        .map(|(_, _, kind, _)| kind)
        .collect()
    }

    fn generic_phase(key: &(AgentKind, String), category: AlertCategory) -> AlertPhase {
        let guard = liveness().lock().unwrap();
        match category {
            AlertCategory::RegistrationLoss => guard[key].registered_alert_phase,
            _ => guard[key].gm_connection_alert_phase,
        }
    }

    /// The generation the `Failure` was decided against, as dispatch would carry it.
    fn generic_generation(key: &(AgentKind, String), category: AlertCategory) -> Instant {
        let guard = liveness().lock().unwrap();
        match category {
            AlertCategory::RegistrationLoss => guard[key].registered_in_flight.generation,
            _ => guard[key].gm_connection_in_flight.generation,
        }
        .unwrap()
    }

    fn check_recovery_mid_send(category: AlertCategory, m: &str, delivered: bool) {
        let cfg = zero_cfg();
        let key = (AgentKind::Ims, m.to_string());
        assert_eq!(
            generic_report(&cfg, category, m, 1, false),
            vec![CriticalEventKind::Failure]
        );
        assert_eq!(generic_phase(&key, category), AlertPhase::Pending);
        let generation = generic_generation(&key, category);

        // Recovers before the send answers: nothing decided, still in flight.
        assert!(generic_report(&cfg, category, m, 2, true).is_empty());
        assert_eq!(generic_phase(&key, category), AlertPhase::Pending);

        // The callback still matches, and owes the all-clear only if delivered.
        assert_eq!(
            record_alert_outcome(&key, category, generation, delivered),
            delivered
        );
        if delivered {
            assert_eq!(generic_phase(&key, category), AlertPhase::Pending);
            finish_late_all_clear(&key, category);
        }
        assert_eq!(generic_phase(&key, category), AlertPhase::Idle);

        // The next outage alerts again.
        assert_eq!(
            generic_report(&cfg, category, m, 3, false),
            vec![CriticalEventKind::Failure]
        );
    }

    #[test]
    fn registration_loss_recovered_mid_send_is_all_cleared() {
        check_recovery_mid_send(AlertCategory::RegistrationLoss, "t138-reg-ok", true);
    }

    #[test]
    fn registration_loss_recovered_mid_send_undelivered_goes_idle() {
        check_recovery_mid_send(AlertCategory::RegistrationLoss, "t138-reg-fail", false);
    }

    #[test]
    fn gm_connection_recovered_mid_send_is_all_cleared() {
        check_recovery_mid_send(AlertCategory::GmConnectionLost, "t138-gm-ok", true);
    }

    #[test]
    fn gm_connection_recovered_mid_send_undelivered_goes_idle() {
        check_recovery_mid_send(AlertCategory::GmConnectionLost, "t138-gm-fail", false);
    }

    #[test]
    fn re_outage_before_callback_does_not_alert_until_old_send_resolves() {
        let cfg = zero_cfg();
        let category = AlertCategory::RegistrationLoss;
        let m = "t138-reoutage";
        let key = (AgentKind::Ims, m.to_string());
        generic_report(&cfg, category, m, 1, false);
        let generation = generic_generation(&key, category);
        generic_report(&cfg, category, m, 2, true);
        // A new outage starts while the old send is unresolved: held back.
        assert!(generic_report(&cfg, category, m, 3, false).is_empty());
        assert!(record_alert_outcome(&key, category, generation, true));
        finish_late_all_clear(&key, category);
        // The new outage is then decided fresh.
        assert_eq!(
            generic_report(&cfg, category, m, 4, false),
            vec![CriticalEventKind::Failure]
        );
    }
}
