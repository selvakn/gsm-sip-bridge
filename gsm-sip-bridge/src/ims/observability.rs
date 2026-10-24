//! Agent A's (`ims::agent`) side of restoring call observability under
//! VoWiFi (specs/014-vowifi-metrics-restore). One `AgentObservability`
//! instance lives for the process's lifetime, wrapping the
//! `observability::reporter::Reporter` that carries events to the daemon
//! and (best-effort) the `StoreHandle` that writes call history rows
//! directly, independent of whether the daemon is reachable.
//!
//! All gauge state (`active_calls`, `registered`, `tunnel_up`) is cached
//! here and re-sent in full on every report, since `AgentState` is
//! "absolute, latest-wins" on the wire (contracts/observability-protocol.md)
//! — a caller only ever needs to say what changed, not reconstruct the rest.

use crate::control::protocol::{
    ActiveCallInfoWire, AgentDetails, AgentState, BridgeFailureReason, CallStatus, ObservedEvent,
    RegistrationStatus,
};
use crate::observability::reporter::Reporter;
use crate::store::calls::CallRecord;
use crate::store::{StoreCommand, StoreHandle, Transport};
use crate::vowifi::control::reason;
use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};

/// What the web UI shows about this line beyond the gauges
/// (specs/122-management-webui): registration wording and age, carrier, the
/// latest signal reading and the calls in progress.
///
/// Read by the reporter's thread at send time, through the provider
/// [`AgentObservability::new`] installs, so every report and heartbeat
/// carries a current copy and no call site has to remember to push these.
#[derive(Default)]
struct DetailsSource {
    status: Mutex<Option<Arc<Mutex<crate::ims::RegistrationStatus>>>>,
    plmn: Mutex<Option<String>>,
    signal: Mutex<Option<crate::volte::sms::SignalSlot>>,
    /// Calls reported as plain figures.
    calls: Mutex<Vec<ActiveCallInfoWire>>,
    /// Calls whose quality is read from the live RTCP state at report time.
    live: Mutex<Vec<LiveCall>>,
}

/// A call in progress, holding the shared counters its quality is read from.
///
/// The figures are read when a report is built, not pushed: the call loop does
/// not have to remember to update anything, and the web UI sees loss and
/// jitter move during the call instead of only at teardown.
pub struct LiveCall {
    pub remote: String,
    pub direction: crate::control::protocol::CallDirectionWire,
    pub started_at_unix: u64,
    pub answered: bool,
    /// The carrier leg's receive tracker and clock rate. `None` on a call
    /// with no RTCP endpoint, whose quality is then unknown, not zero.
    pub receive: Option<(Arc<Mutex<crate::ims::media_stats::ReceiveTracker>>, u32)>,
    pub far_end: Option<Arc<Mutex<crate::ims::rtcp::FarEndQuality>>>,
}

impl LiveCall {
    fn to_wire(&self) -> ActiveCallInfoWire {
        let local = self.receive.as_ref().and_then(|(tracker, clock_rate)| {
            tracker
                .lock()
                .ok()
                .map(|t| t.stats(*clock_rate))
                .filter(|s| s.received_packets > 0 || s.lost_packets > 0)
        });
        let rtt = self
            .far_end
            .as_ref()
            .and_then(|fe| fe.lock().ok().and_then(|fe| fe.round_trip));
        ActiveCallInfoWire {
            remote: self.remote.clone(),
            direction: self.direction,
            started_at_unix: self.started_at_unix,
            answered: self.answered,
            loss_percent: local.map(|s| s.loss_percent()),
            jitter_ms: local.map(|s| s.jitter.as_secs_f64() * 1000.0),
            rtt_ms: rtt.map(|r| r.as_secs_f64() * 1000.0),
        }
    }
}

fn unix(t: std::time::SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

impl DetailsSource {
    fn snapshot(&self) -> AgentDetails {
        let mut details = AgentDetails::default();
        let status = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(status) = status {
            let status = status.lock().unwrap_or_else(|e| e.into_inner());
            details.registration_state = Some(status.state.into());
            if matches!(
                status.state,
                crate::ims::RegistrationState::Registered | crate::ims::RegistrationState::Renewing
            ) {
                details.registered_at = status.registered_at.and_then(unix);
            }
        }
        details.plmn = self.plmn.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let slot = self
            .signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(slot) = slot {
            details.signal = slot
                .reading
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(Into::into);
        }
        let mut calls = self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        calls.extend(
            self.live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(LiveCall::to_wire),
        );
        details.with_active_calls(calls)
    }
}

pub struct AgentObservability {
    reporter: Reporter,
    module_id: String,
    /// `None` when the store could not be opened — reporting must never be
    /// able to take an agent down (FR-018), so a missing store degrades to
    /// "no history for this run" rather than a startup failure.
    store: Option<StoreHandle>,
    /// Reused for every call record (`[bridge].sip_destination`) — the same
    /// destination Agent B dials, since there is exactly one PBX destination
    /// for the whole bridge.
    sip_destination: String,
    /// Which transport this agent's call rows are filed under. Both the VoWiFi
    /// and VoLTE paths run this same code, so it must be told which it is or
    /// their history collapses into one — the store counterpart of the metric
    /// `transport` label.
    transport: Transport,
    state: Mutex<AgentState>,
    /// Where the active-call count is mirrored for this line's tunnel
    /// supervisor, which holds a stuck-rekey rebuild for a call (specs/093
    /// `contracts/files.md`). VoWiFi only; `None` on VoLTE.
    calls_file: Option<std::path::PathBuf>,
    /// Calls being set up, which [`Self::calls_file`] counts as occupying the
    /// line though the `active_calls` gauge does not: an inbound call ringing
    /// or an outbound one being placed would be cut by a rebuild as surely as
    /// a connected one.
    setup: Mutex<CallSetup>,
    details: Arc<DetailsSource>,
    /// The last call's quality figures, held until its history row is written
    /// (specs/122-management-webui). They are reported as events just before
    /// the row, and the row is the only place they become per-call data.
    quality: Mutex<CallQuality>,
}

/// What [`AgentObservability::report_media_quality`] last saw, per source.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct CallQuality {
    /// Loss and jitter (seconds) this bridge measured on what it received.
    local: Option<(f64, f64)>,
    /// Round-trip time (seconds) from the far end's RTCP.
    remote_rtt: Option<f64>,
}

impl CallQuality {
    /// `(loss %, jitter ms, rtt ms)` as stored in the `calls` table.
    fn columns(self) -> (Option<f64>, Option<f64>, Option<f64>) {
        (
            self.local.map(|(loss, _)| loss),
            self.local.map(|(_, jitter)| jitter * 1000.0),
            self.remote_rtt.map(|rtt| rtt * 1000.0),
        )
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct CallSetup {
    inbound: bool,
    outbound: bool,
    /// What the calls file was last written with, so an unchanged state is
    /// not rewritten on every dispatch-loop pass.
    written: Option<u32>,
}

impl AgentObservability {
    pub fn new(
        reporter: Reporter,
        module_id: String,
        store: Option<StoreHandle>,
        sip_destination: String,
        transport: Transport,
    ) -> Self {
        let details = Arc::new(DetailsSource::default());
        let for_reporter = Arc::clone(&details);
        reporter.set_details_provider(Arc::new(move || for_reporter.snapshot()));
        Self {
            reporter,
            module_id,
            store,
            sip_destination,
            transport,
            state: Mutex::new(AgentState::default()),
            calls_file: None,
            setup: Mutex::new(CallSetup::default()),
            details,
            quality: Mutex::new(CallQuality::default()),
        }
    }

    /// Reports this registration's wording and age from `status`, the
    /// dispatch loop's own record -- the same one `vowifi-status` answers
    /// from, so the web UI and the CLI cannot disagree (FR-012).
    pub fn attach_registration_status(&self, status: Arc<Mutex<crate::ims::RegistrationStatus>>) {
        *self
            .details
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(status);
    }

    /// The home network as `"MCC-MNC"`.
    pub fn set_plmn(&self, mcc: &str, mnc: &str) {
        *self.details.plmn.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("{mcc}-{mnc}"));
    }

    /// Reports whatever the modem sweep leaves in `slot`.
    pub fn attach_signal_slot(&self, slot: crate::volte::sms::SignalSlot) {
        *self
            .details
            .signal
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(slot);
    }

    /// A call has been bridged: show it as in progress until
    /// `set_active_calls(0)`.
    pub fn begin_live_call(&self, call: LiveCall) {
        let mut live = self.details.live.lock().unwrap_or_else(|e| e.into_inner());
        live.clear();
        live.push(call);
    }

    /// Replaces the calls-in-progress list reported to the web UI.
    pub fn set_active_call_details(&self, calls: Vec<ActiveCallInfoWire>) {
        *self.details.calls.lock().unwrap_or_else(|e| e.into_inner()) = calls;
    }

    /// Also mirror every active-call count into `path`.
    pub fn with_calls_file(mut self, path: std::path::PathBuf) -> Self {
        self.calls_file = Some(path);
        self
    }

    fn push(&self, events: Vec<ObservedEvent>) {
        let state = *self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.reporter.report(state, events);
    }

    pub fn set_active_calls(&self, n: u32) {
        if n == 0 {
            // No call left to describe.
            self.details
                .live
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_calls = Some(n);
        self.sync_calls_file();
        self.push(Vec::new());
    }

    /// An inbound call is ringing (`true`) or its setup has ended either way.
    pub fn set_inbound_setup(&self, on: bool) {
        self.setup.lock().unwrap_or_else(|e| e.into_inner()).inbound = on;
        self.sync_calls_file();
    }

    /// An outbound call is being placed (`true`) or is not. Cheap to call on
    /// every dispatch-loop pass: the file is only rewritten on a change.
    pub fn set_outbound_setup(&self, on: bool) {
        self.setup
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outbound = on;
        self.sync_calls_file();
    }

    /// Mirrors "how many calls occupy this line" (connected plus being set
    /// up) into the calls file, for the tunnel supervisor (specs/093).
    fn sync_calls_file(&self) {
        let active = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active_calls
            .unwrap_or(0);
        let mut setup = self.setup.lock().unwrap_or_else(|e| e.into_inner());
        let n = active.max(u32::from(setup.inbound || setup.outbound));
        // The signal poll stands aside while the line is occupied. This runs
        // on VoLTE too, which has no calls file.
        if let Some(slot) = self
            .details
            .signal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            slot.call_active
                .store(n > 0, std::sync::atomic::Ordering::Relaxed);
        }
        let Some(path) = &self.calls_file else {
            return;
        };
        if setup.written == Some(n) {
            return;
        }
        // Best-effort like every other report: a write that fails costs only
        // the deferral, and the supervisor reads a missing file as "no call".
        match crate::ims::pcscf_files::write_active_calls(path, n) {
            Ok(()) => setup.written = Some(n),
            Err(e) => {
                tracing::debug!(error = %e, path = %path.display(), "could not write the active-call count")
            }
        }
    }

    pub fn set_registered(&self, registered: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .registered = Some(registered);
        self.push(Vec::new());
    }

    /// Report when this line's registration lapses
    /// (specs/039-at-stall-watchdog, FR-018).
    ///
    /// Absolute, and reported only when it changes — once per renewal. The
    /// daemon turns it into a countdown at scrape time, so an operator can see
    /// at a glance that a binding lapsed hours ago rather than inferring it
    /// from a raw timestamp nobody converts by hand.
    pub fn set_registration_expiry(&self, expires_at: std::time::SystemTime) {
        let unix = expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .registration_expires_at = Some(unix);
        self.push(Vec::new());
    }

    pub fn set_tunnel_up(&self, up: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tunnel_up = Some(up);
        self.push(Vec::new());
    }

    /// Report the Gm signaling connection's health (specs/028). Called on
    /// state changes only — never per idle poll — so the reporter isn't asked
    /// to coalesce a steady stream of identical values.
    pub fn set_gm_connection_up(&self, up: bool) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .gm_connection_up = Some(up);
        self.push(Vec::new());
    }

    pub fn report_registration_attempt(&self, status: RegistrationStatus) {
        self.push(vec![ObservedEvent::RegistrationAttempt { status }]);
    }

    /// A call that never reached an answer: declined outright, the PBX
    /// leg failed to establish, the extension rang out, or the caller gave
    /// up while it rang. `status` is `Missed` when the PBX extension was
    /// actually reached and rang (a human could have picked up), `Failed`
    /// when the call never got that far (no usable codec, Agent B
    /// unreachable, an internal error).
    pub fn report_call_not_answered(
        &self,
        status: CallStatus,
        reason: BridgeFailureReason,
        caller: &str,
        started_at: DateTime<Utc>,
    ) {
        self.push(vec![
            ObservedEvent::CallCompleted {
                status,
                duration_seconds: 0.0,
            },
            ObservedEvent::BridgeFailed { reason },
        ]);
        self.insert_call_row(
            caller,
            started_at,
            0.0,
            status,
            crate::store::calls::CallDirection::Inbound,
        );
    }

    /// Reports a call that reached an answer and then ended, judged by its
    /// media (FR-017): a call that carried audio both ways is `Answered`; one
    /// that carried one-way or no audio is a **failure**, recorded as `Failed`
    /// with the direction named, never as a success.
    pub fn report_call_answered_and_ended(
        &self,
        caller: &str,
        started_at: DateTime<Utc>,
        duration_seconds: f64,
        media: super::media_stats::DirectionVerdict,
        direction: crate::store::calls::CallDirection,
    ) {
        if media.is_success() {
            self.push(vec![ObservedEvent::CallCompleted {
                status: CallStatus::Answered,
                duration_seconds,
            }]);
            self.insert_call_row(
                caller,
                started_at,
                duration_seconds,
                CallStatus::Answered,
                direction,
            );
        } else {
            self.push(vec![
                ObservedEvent::CallCompleted {
                    status: CallStatus::Failed,
                    duration_seconds,
                },
                ObservedEvent::BridgeFailed {
                    reason: BridgeFailureReason::OneWayAudio,
                },
            ]);
            self.insert_call_row(
                caller,
                started_at,
                duration_seconds,
                CallStatus::Failed,
                direction,
            );
        }
    }

    /// Reports one source's media-quality figures for a just-ended call
    /// (specs/046-rtcp-reporting FR-008a) — call once per source that
    /// actually has something to report; a source that never produced any
    /// figures (the far end never sent RTCP, or no media was received at
    /// all) is simply not reported, rather than reported as zero (FR-009's
    /// "never reported" distinction, carried through to the metrics
    /// surface as well as the log line).
    pub fn report_media_quality(
        &self,
        source: crate::control::protocol::QualitySource,
        loss_percent: f64,
        jitter_seconds: f64,
        round_trip_seconds: Option<f64>,
    ) {
        {
            let mut q = self.quality.lock().unwrap_or_else(|e| e.into_inner());
            match source {
                crate::control::protocol::QualitySource::Local => {
                    q.local = Some((loss_percent, jitter_seconds));
                }
                crate::control::protocol::QualitySource::Remote => {
                    q.remote_rtt = round_trip_seconds;
                }
            }
        }
        self.push(vec![ObservedEvent::MediaQuality {
            source,
            loss_percent,
            jitter_seconds,
            round_trip_seconds,
        }]);
    }

    /// A call proceeded with no RTCP endpoint obtainable at all (FR-017a).
    pub fn report_rtcp_unavailable(&self) {
        self.push(vec![ObservedEvent::RtcpUnavailable]);
    }

    fn insert_call_row(
        &self,
        caller: &str,
        started_at: DateTime<Utc>,
        duration_seconds: f64,
        status: CallStatus,
        direction: crate::store::calls::CallDirection,
    ) {
        let Some(store) = &self.store else {
            return;
        };
        // Taken, so one call's figures can never land on the next call's row.
        let (loss_percent, jitter_ms, rtt_ms) =
            std::mem::take(&mut *self.quality.lock().unwrap_or_else(|e| e.into_inner())).columns();
        let record = CallRecord {
            module_id: self.module_id.clone(),
            caller_id: caller.to_string(),
            started_at: started_at.to_rfc3339(),
            duration_seconds,
            status: status.as_str().to_string(),
            sip_destination: self.sip_destination.clone(),
            transport: self.transport,
            direction,
            loss_percent,
            jitter_ms,
            rtt_ms,
        };
        if let Err(e) = store.sender().send(StoreCommand::InsertCall(record)) {
            tracing::error!(error = %e, "failed to send call record to store");
        }
    }
}

/// Maps the free-form reason strings carried by `vowifi::control`'s
/// `ControlMessage::BridgeFailed`/`CallEnded` (`vowifi::control::reason`)
/// onto the closed `BridgeFailureReason` set (FR-014) — new carrier-facing
/// failure text must never be able to mint a new metric label; anything
/// unrecognised collapses to `BridgeSetupFailed`.
pub fn map_bridge_failure_reason(raw: &str) -> BridgeFailureReason {
    match raw {
        // Only a genuine decline is `pbx_declined` (specs/084 FR-007a): a PBX
        // that could not even be reached at setup is a setup failure, and a
        // PBX leg that died before answer is `pbx_failed`, not a busy human.
        reason::PBX_REJECTED => BridgeFailureReason::PbxDeclined,
        reason::PBX_UNREACHABLE => BridgeFailureReason::BridgeSetupFailed,
        reason::PBX_FAILED => BridgeFailureReason::PbxFailed,
        reason::PBX_NO_ANSWER => BridgeFailureReason::RingTimeout,
        reason::CALLER_CANCELLED => BridgeFailureReason::CallerCancelled,
        reason::VETH_LEG_FAILED => BridgeFailureReason::BridgeSetupFailed,
        reason::TRANSPORT_ERROR => BridgeFailureReason::AgentUnreachable,
        _ => BridgeFailureReason::BridgeSetupFailed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_for_details() -> AgentObservability {
        AgentObservability::new(
            Reporter::spawn(
                "/nonexistent/no-daemon.sock".to_string(),
                crate::control::protocol::AgentKind::Ims,
                "card-details".into(),
                std::time::Duration::from_secs(3600),
            ),
            "card-details".into(),
            None,
            "sip:pbx@192.0.2.1".into(),
            Transport::Vowifi,
        )
    }

    #[test]
    fn details_carry_registration_wording_plmn_signal_and_calls() {
        use crate::control::protocol::CallDirectionWire;
        let obs = obs_for_details();
        let status = Arc::new(Mutex::new(crate::ims::RegistrationStatus {
            state: crate::ims::RegistrationState::Registered,
            registered_at: Some(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
            ),
            ..Default::default()
        }));
        obs.attach_registration_status(Arc::clone(&status));
        obs.set_plmn("405", "857");
        let slot: crate::volte::sms::SignalSlot =
            Arc::new(crate::volte::sms::SignalShare::default());
        obs.attach_signal_slot(Arc::clone(&slot));

        let d = obs.details.snapshot();
        assert_eq!(d.registration_state.unwrap().to_string(), "registered");
        assert_eq!(d.registered_at, Some(1_700_000_000));
        assert_eq!(d.plmn.as_deref(), Some("405-857"));
        assert!(d.signal.is_none(), "nothing read yet");
        assert_eq!(d.active_call_details, Some(vec![]));

        *slot.reading.lock().unwrap() = Some(crate::modules::signal::SignalReading {
            at_unix: 5,
            rssi_dbm: Some(-71),
            ..Default::default()
        });
        obs.set_active_call_details(vec![ActiveCallInfoWire {
            remote: "+919000000001".into(),
            direction: CallDirectionWire::Inbound,
            started_at_unix: 1,
            answered: true,
            loss_percent: None,
            jitter_ms: None,
            rtt_ms: None,
        }]);
        // The signal poll is told when the line is occupied.
        assert!(!slot.is_call_active());
        obs.set_inbound_setup(true);
        assert!(slot.is_call_active(), "a ringing call holds the poll back");
        obs.set_inbound_setup(false);
        obs.set_active_calls(1);
        assert!(slot.is_call_active());
        obs.set_active_calls(0);
        assert!(!slot.is_call_active());

        // The registration moving on is picked up with no further call.
        status.lock().unwrap().state = crate::ims::RegistrationState::Failed;

        let d = obs.details.snapshot();
        assert_eq!(d.signal.unwrap().rssi_dbm, Some(-71));
        assert_eq!(d.active_call_details.unwrap().len(), 1);
        assert_eq!(d.registration_state.unwrap().to_string(), "failed");
        assert_eq!(
            d.registered_at, None,
            "a failed line has no registered-since"
        );
    }

    #[test]
    fn a_live_calls_quality_is_read_fresh_and_cleared_when_it_ends() {
        use crate::control::protocol::CallDirectionWire;
        use crate::ims::media_stats::ReceiveTracker;
        let obs = obs_for_details();
        let tracker = Arc::new(Mutex::new(ReceiveTracker::new()));
        let far_end = Arc::new(Mutex::new(crate::ims::rtcp::FarEndQuality::default()));
        obs.begin_live_call(LiveCall {
            remote: "+919000000001".into(),
            direction: CallDirectionWire::Inbound,
            started_at_unix: 1,
            answered: true,
            receive: Some((Arc::clone(&tracker), 8000)),
            far_end: Some(Arc::clone(&far_end)),
        });

        // Nothing received yet: unknown, never 0.
        let call = &obs.details.snapshot().active_call_details.unwrap()[0];
        assert_eq!(call.loss_percent, None);
        assert_eq!(call.jitter_ms, None);
        assert_eq!(call.rtt_ms, None);

        // Packets 1, 2 and 4 arrive: one lost of four expected.
        {
            let mut t = tracker.lock().unwrap();
            for (seq, ts) in [(1u16, 160u32), (2, 320), (4, 640)] {
                t.on_packet(
                    seq,
                    ts,
                    std::time::Duration::from_millis(u64::from(ts) / 8),
                    8000,
                );
            }
        }
        far_end.lock().unwrap().round_trip = Some(std::time::Duration::from_millis(80));
        let call = &obs.details.snapshot().active_call_details.unwrap()[0];
        assert_eq!(call.loss_percent, Some(25.0));
        assert!(call.jitter_ms.is_some());
        assert!((call.rtt_ms.unwrap() - 80.0).abs() < 1e-6);

        obs.set_active_calls(0);
        assert_eq!(obs.details.snapshot().active_call_details, Some(vec![]));
    }

    #[test]
    fn calls_in_progress_or_being_set_up_are_mirrored_into_the_calls_file() {
        let dir = std::env::temp_dir().join(format!("obs-calls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pcscf-0.calls");
        let obs = AgentObservability::new(
            Reporter::spawn(
                dir.join("no-daemon.sock").display().to_string(),
                crate::control::protocol::AgentKind::Ims,
                "card0".into(),
                std::time::Duration::from_secs(3600),
            ),
            "card0".into(),
            None,
            "sip:pbx@192.0.2.1".into(),
            Transport::Vowifi,
        )
        .with_calls_file(path.clone());
        let read = || std::fs::read_to_string(&path).unwrap();
        obs.set_active_calls(1);
        assert_eq!(read(), "1\n");
        obs.set_active_calls(0);
        assert_eq!(read(), "0\n");
        // A call being set up occupies the line before the gauge counts it.
        obs.set_inbound_setup(true);
        assert_eq!(read(), "1\n", "ringing");
        obs.set_active_calls(1);
        obs.set_inbound_setup(false);
        assert_eq!(read(), "1\n", "answered: still occupied");
        obs.set_active_calls(0);
        assert_eq!(read(), "0\n");
        obs.set_outbound_setup(true);
        assert_eq!(read(), "1\n", "dialing");
        obs.set_outbound_setup(false);
        assert_eq!(read(), "0\n", "the attempt failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_bridge_failure_reason_covers_every_known_reason_constant() {
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_UNREACHABLE),
            BridgeFailureReason::BridgeSetupFailed
        );
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_FAILED),
            BridgeFailureReason::PbxFailed
        );
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_REJECTED),
            BridgeFailureReason::PbxDeclined
        );
        assert_eq!(
            map_bridge_failure_reason(reason::PBX_NO_ANSWER),
            BridgeFailureReason::RingTimeout
        );
        assert_eq!(
            map_bridge_failure_reason(reason::CALLER_CANCELLED),
            BridgeFailureReason::CallerCancelled
        );
        assert_eq!(
            map_bridge_failure_reason(reason::VETH_LEG_FAILED),
            BridgeFailureReason::BridgeSetupFailed
        );
        assert_eq!(
            map_bridge_failure_reason(reason::TRANSPORT_ERROR),
            BridgeFailureReason::AgentUnreachable
        );
    }

    #[test]
    fn map_bridge_failure_reason_falls_back_for_unknown_strings() {
        assert_eq!(
            map_bridge_failure_reason("some_future_reason_never_seen_before"),
            BridgeFailureReason::BridgeSetupFailed
        );
    }
}
