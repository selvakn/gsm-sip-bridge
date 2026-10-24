use crate::modules::at_commander::NetworkMode;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

fn default_restart_mode() -> String {
    "full".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControlCmd {
    CardRestart {
        slot: u32,
        /// `"full"` (default) or `"radio"` — same two values and meaning as
        /// `[scheduled_restart].restart_mode`; validated server-side
        /// (`modules::CardPool::handle_control_cmd`), same pattern as
        /// `SetMode`'s own `mode` string. Defaulted so an older client that
        /// predates this field still round-trips.
        #[serde(default = "default_restart_mode")]
        mode: String,
    },
    SetMode {
        slot: u32,
        mode: String,
    },
    GetMode {
        slot: u32,
    },
    ListSlots,
    /// A VoWiFi agent (`ims::agent` or `vowifi::mod`) reporting call/SMS
    /// events and current gauge state (specs/014-vowifi-metrics-restore).
    /// Routed straight to `metrics::ingest::apply_report` by
    /// `control::server::handle_connection`, never reaching `CardPool`'s
    /// mailbox — see contracts/observability-protocol.md.
    Observe {
        /// Boxed: with `AgentReport::details` it dwarfs every other command,
        /// and this enum is sent by value through the control channel.
        report: Box<AgentReport>,
    },
}

/// Which VoWiFi agent sent an `AgentReport`. Liveness (`AGENT_UP`) is
/// tracked per kind, independently of `module_id` — a card can be replaced
/// or fail to resolve, but the agent process identity is always one of
/// these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// Agent A: `ims::agent`, runs inside the ePDG tunnel's `ims` netns.
    Ims,
    /// Agent B: `vowifi::mod`, runs in the default netns.
    Sip,
    /// The carrier side of the host-side cellular bridging service
    /// (specs/017-volte-inbound-bridge) — `ims::agent`, but over LTE rather
    /// than an ePDG tunnel. Distinct from `Ims` **only** so its calls carry
    /// the right `transport` label: both run the same agent code, but a VoLTE
    /// call reported as `vowifi` would be invisible in exactly the comparison
    /// this feature exists to make.
    Volte,
    /// The telephone-system side of that same VoLTE bridge — the identical
    /// `vowifi::mod` code `Sip` runs, but as a half of the cellular service,
    /// so its PBX-leg outcomes must be labelled `volte`, not `vowifi`. This
    /// is to `Volte` what `Sip` is to `Ims`.
    ///
    /// It is a separate kind from `Volte` (not just a shared label) because
    /// the two halves run as **independent reporters** with their own
    /// `epoch`/`seq` counters; sharing one `(kind, module_id)` liveness key
    /// would let each erase the record the other needs to detect a replay,
    /// re-applying already-counted events. See `metrics::ingest::liveness`.
    VolteSip,
    /// The `supervise` process, reporting per-line tunnel state it alone can
    /// observe (specs/086). Reports only `child_sa_up`.
    Supervisor,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Ims => "ims",
            AgentKind::Sip => "sip",
            AgentKind::Volte => "volte",
            AgentKind::VolteSip => "volte_sip",
            AgentKind::Supervisor => "supervisor",
        }
    }
}

/// One message an agent sends over the observability protocol: absolute
/// gauge state (always present, which is what makes an empty-`events`
/// report a heartbeat) plus zero or more counter deltas since the last
/// successfully delivered report.
///
/// `epoch`/`seq` make a report idempotent to replay: the reporter's
/// send/retry loop is single-threaded per agent (one report in flight at a
/// time), so if the daemon applies a report but the acknowledgement is lost
/// — a torn connection right as the response was written — the reporter
/// retries the *same* report rather than knowing it already landed. `seq` is
/// a per-agent-process counter assigned once when a report is enqueued and
/// kept across retries of that same report; `epoch` is a random value fixed
/// for the reporter's process lifetime, so a restarted agent's `seq`
/// resetting to 1 is never mistaken for a replay of a previous run's
/// already-applied `seq` values (`metrics::ingest` only compares `seq`
/// within a matching `epoch`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentReport {
    pub agent: AgentKind,
    pub module_id: String,
    pub epoch: u64,
    pub seq: u64,
    pub state: AgentState,
    #[serde(default)]
    pub events: Vec<ObservedEvent>,
    #[serde(default)]
    pub dropped: u64,
    /// Display-only extras for the management web UI
    /// (specs/122-management-webui, contracts/agent-report.md). A sibling of
    /// `state` rather than part of it because `AgentState` is `Copy` and
    /// these fields own strings and a list. An older peer omits it; an older
    /// daemon ignores it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<AgentDetails>,
}

/// Most `active_call_details` entries one report carries, to keep reports
/// bounded.
pub const MAX_ACTIVE_CALL_DETAILS: usize = 4;

/// See [`AgentReport::details`]. Every field is optional: a `Some` overwrites
/// what the daemon holds, a `None` leaves it as it was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentDetails {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registration_state: Option<RegistrationStateWire>,
    /// Unix seconds of the current registration.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registered_at: Option<u64>,
    /// `"MCC-MNC"`, e.g. `"405-857"`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub plmn: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub signal: Option<SignalReadingWire>,
    /// `Some(vec![])` means "no calls in progress".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_call_details: Option<Vec<ActiveCallInfoWire>>,
}

impl AgentDetails {
    /// Sets the in-progress call list, truncated to
    /// [`MAX_ACTIVE_CALL_DETAILS`].
    pub fn with_active_calls(mut self, mut calls: Vec<ActiveCallInfoWire>) -> Self {
        calls.truncate(MAX_ACTIVE_CALL_DETAILS);
        self.active_call_details = Some(calls);
        self
    }
}

/// Wire mirror of `ims::RegistrationState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStateWire {
    Unregistered,
    Registering,
    Registered,
    Renewing,
    Failed,
}

impl From<crate::ims::RegistrationState> for RegistrationStateWire {
    fn from(state: crate::ims::RegistrationState) -> Self {
        use crate::ims::RegistrationState as S;
        match state {
            S::Unregistered => Self::Unregistered,
            S::Registering => Self::Registering,
            S::Registered => Self::Registered,
            S::Renewing => Self::Renewing,
            S::Failed => Self::Failed,
        }
    }
}

/// The same wording `vowifi-status` prints (`volte::registration::state_label`).
impl std::fmt::Display for RegistrationStateWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unregistered => "unregistered",
            Self::Registering => "registering",
            Self::Registered => "registered",
            Self::Renewing => "renewing",
            Self::Failed => "failed",
        })
    }
}

/// One modem signal reading (`modules::signal::SignalReading` on the wire).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SignalReadingWire {
    pub at_unix: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rssi_dbm: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rat: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rsrp_dbm: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rsrq_db: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sinr_db: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirectionWire {
    Inbound,
    Outbound,
}

/// A call in progress on a line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveCallInfoWire {
    /// The caller, or the dialled number.
    pub remote: String,
    pub direction: CallDirectionWire,
    pub started_at_unix: u64,
    /// Ringing (`false`) versus connected (`true`).
    pub answered: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub loss_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub jitter_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub rtt_ms: Option<f64>,
}

/// Why a line's IKE_SA vanished, as far as the charon log can tell
/// (specs/087). Closed on purpose: it only ever feeds log lines and alert
/// text, so neither the wire nor the text can grow unbounded cardinality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IkeSaLossCause {
    /// The ePDG sent an IKE `DELETE` for the IKE_SA.
    CarrierDelete,
    /// charon gave up retransmitting to an unresponsive peer.
    DeadPeer,
    /// Neither was logged (e.g. a loss we caused ourselves).
    Unknown,
}

impl std::fmt::Display for IkeSaLossCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CarrierDelete => "carrier DELETE",
            Self::DeadPeer => "dead-peer timeout",
            Self::Unknown => "unknown",
        })
    }
}

/// Whether a dead-peer IKE_SA loss happened while the ePDG was demonstrably
/// alive (specs/098 FR-008), judged by the line supervisor from inbound ESP.
/// Closed: it is a metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadPeerVerdict {
    /// Inbound ESP arrived while charon was still retransmitting: the path
    /// was working again before charon gave up. Either the ePDG ignored an IKE
    /// request on a working tunnel (#120 finding 2), or a local outage ended
    /// after charon's last retransmit (specs/098 research R7).
    False,
    /// Nothing arrived after the first unanswered request could have gone out.
    Real,
    /// No snapshot, or one that cannot place the last inbound packet on either
    /// side of the window.
    Unknown,
    /// The loss was seen while the line was held for an uplink outage
    /// (specs/100 FR-022): the peer's silence is explained by the path, so it
    /// is never `real`.
    Uplink,
}

impl DeadPeerVerdict {
    pub const ALL: [Self; 4] = [Self::False, Self::Real, Self::Unknown, Self::Uplink];

    /// The wire and metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::False => "false",
            Self::Real => "real",
            Self::Unknown => "unknown",
            Self::Uplink => "uplink",
        }
    }
}

/// What the underlay guard believes about the path to a line's ePDG
/// (specs/100). Closed: it is a wire value and a metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkState {
    Up,
    /// The host has no usable route (or source address) to the ePDG.
    NoRoute,
    /// The route is fine, but the ePDG has stopped answering.
    NoResponse,
}

impl UplinkState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::NoRoute => "no_route",
            Self::NoResponse => "no_response",
        }
    }
}

/// Why a line is held (specs/100): the two non-`up` [`UplinkState`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UplinkHoldCause {
    NoRoute,
    NoResponse,
}

impl UplinkHoldCause {
    pub const ALL: [Self; 2] = [Self::NoRoute, Self::NoResponse];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoRoute => "no_route",
            Self::NoResponse => "no_response",
        }
    }
}

impl From<UplinkHoldCause> for UplinkState {
    fn from(cause: UplinkHoldCause) -> Self {
        match cause {
            UplinkHoldCause::NoRoute => Self::NoRoute,
            UplinkHoldCause::NoResponse => Self::NoResponse,
        }
    }
}

/// What ended a hold (specs/100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldEndedBy {
    /// The path came back.
    Restored,
    /// The 15-minute bound was reached.
    MaxHold,
    /// The supervisor was shutting down.
    Shutdown,
    /// The path is still down, for another reason: the hold carries on under
    /// the new cause, and its 15-minute bound still runs from its start.
    CauseChanged,
}

impl HoldEndedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restored => "restored",
            Self::MaxHold => "max_hold",
            Self::Shutdown => "shutdown",
            Self::CauseChanged => "cause_changed",
        }
    }
}

/// Why the line supervisor recovered a strongSwan VoWiFi line on a steady-state
/// tick (specs/096). Mirrors `supervise::line_supervisor::DegradeReason`
/// one-to-one without depending on it, the same layering `CallStatus` and
/// `OutboundAttemptOutcome` keep; the mapping lives on the supervisor side and
/// is exhaustive, so a new reason cannot ship without a label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelRecoveryReason {
    ProcessDied,
    ViciBroken,
    TunVanished,
    TunUnavailable,
    ChildSaMissing,
    IkeSaLost,
    ChildSaStuck,
    ChildSaRekeyStuck,
    /// Never a steady-state recovery; present only so the mapping is total.
    PcscfChanged,
    PcscfUnreachable,
    DefaultRouteMissing,
    /// The agent marked every P-CSCF in the pool since the last negotiation
    /// (specs/097).
    PcscfPoolExhausted,
    /// A half-open IKE_SA stopped showing any IKE activity (specs/100): charon
    /// is no longer working on it, so it was terminated and re-initiated.
    IkeSaNegotiationStuck,
}

impl TunnelRecoveryReason {
    pub const ALL: [Self; 13] = [
        Self::ProcessDied,
        Self::ViciBroken,
        Self::TunVanished,
        Self::TunUnavailable,
        Self::ChildSaMissing,
        Self::IkeSaLost,
        Self::ChildSaStuck,
        Self::ChildSaRekeyStuck,
        Self::PcscfChanged,
        Self::PcscfUnreachable,
        Self::DefaultRouteMissing,
        Self::PcscfPoolExhausted,
        Self::IkeSaNegotiationStuck,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProcessDied => "process_died",
            Self::ViciBroken => "vici_broken",
            Self::TunVanished => "tun_vanished",
            Self::TunUnavailable => "tun_unavailable",
            Self::ChildSaMissing => "child_sa_missing",
            Self::IkeSaLost => "ike_sa_lost",
            Self::ChildSaStuck => "child_sa_stuck",
            Self::ChildSaRekeyStuck => "child_sa_rekey_stuck",
            Self::PcscfChanged => "pcscf_changed",
            Self::PcscfUnreachable => "pcscf_unreachable",
            Self::DefaultRouteMissing => "default_route_missing",
            Self::PcscfPoolExhausted => "pcscf_pool_exhausted",
            Self::IkeSaNegotiationStuck => "ike_sa_negotiation_stuck",
        }
    }
}

/// Absolute, latest-wins gauge state. `None` means "this agent does not
/// report this signal" — distinct from `Some(false)`, which means "reports
/// it, and it is currently down". The daemon never invents a value for
/// `None` (data-model.md §1).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct AgentState {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub active_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tunnel_up: Option<bool>,
    /// Whether this line's Gm signaling connection is up (specs/028). `None`
    /// means "this agent does not report the signal" — an older peer, or a
    /// report carrying only other fields — and the ingest side leaves the
    /// gauge unchanged rather than reading absent as down (data-model.md §1).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub gm_connection_up: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pbx_registered: Option<bool>,
    /// SIP-server mode only: how many IP phones the registrar this agent hosts
    /// currently holds bindings for.
    ///
    /// Reported rather than exported directly because the registrar is hosted by
    /// whichever process owns the SIP side, and on the VoWiFi/VoLTE paths that is
    /// this agent — which serves no `/metrics`. Gauges registered here would
    /// never be scraped (spec 024; docs/greptile-review-learnings.md, "Metrics &
    /// observability").
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sip_server_bindings: Option<u32>,
    /// SIP-server mode only: whether `[sip_server].ring_aor` specifically has a
    /// live binding — the gauge that answers "will a call ring?".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sip_server_ring_registered: Option<bool>,
    /// When this line's registration lapses, as an absolute unix timestamp
    /// (specs/039-at-stall-watchdog, FR-018).
    ///
    /// Absolute rather than a countdown so it need only be reported when it
    /// changes — once per renewal — with the daemon computing the remaining
    /// seconds at scrape time. Reporting a countdown would mean a report every
    /// second, drowning the bounded reporter channel to say nothing new. This
    /// mirrors how `agent_up`/`last_report_seconds` are already evaluated at
    /// scrape rather than on a timer.
    ///
    /// `None` means the agent does not report it; the ingest side leaves the
    /// gauge untouched rather than reading absent as expired.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub registration_expires_at: Option<u64>,
    /// specs/086: whether the supervisor's last health check confirmed a usable
    /// CHILD_SA for this line. `None` means not reported (every non-supervisor
    /// agent); the ingest side leaves the gauge untouched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub child_sa_up: Option<bool>,
    /// Supervisor only: the line's tunnel has needed `FLAP_REBUILDS` or more
    /// IKE_SA rebuilds within `unhealthy_sec` and has not had a full quiet
    /// window since (specs/087). `None` = not reported; ingest leaves its state
    /// untouched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tunnel_flapping: Option<bool>,
    /// Supervisor only: why the most recent IKE_SA loss on this line happened.
    /// For alert text; never drives a decision.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ike_sa_loss_cause: Option<IkeSaLossCause>,
    /// Supervisor only: what the underlay guard believes about the path to
    /// this line's ePDG (specs/100). Absent when the guard is off, on the swu
    /// engine, and from an older supervisor; ingest then leaves its state
    /// untouched.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub uplink: Option<UplinkState>,
}

/// A counter delta or histogram observation. Every enumerated field below is
/// a closed Rust enum rather than a free string — the mechanism that keeps
/// metric label cardinality bounded regardless of what an agent observes
/// (FR-014).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ObservedEvent {
    CallCompleted {
        status: CallStatus,
        duration_seconds: f64,
    },
    PbxLegCompleted {
        outcome: SmsOutcome,
    },
    BridgeFailed {
        reason: BridgeFailureReason,
    },
    SmsReceived,
    SmsForwarded {
        outcome: SmsOutcome,
    },
    RegistrationAttempt {
        status: RegistrationStatus,
    },
    /// specs/025-outbound-calling. `vowifi-sip-agent`/the VoLTE telephony
    /// half serve no `/metrics` of their own (same reason
    /// `AgentState::sip_server_bindings` is reported rather than exported
    /// directly), so `OUTBOUND_ATTEMPTS_TOTAL` reaches the daemon's endpoint
    /// through this channel for outbound attempts placed over VoWiFi/VoLTE.
    /// CS-path attempts (`modules::mod`, same process as `/metrics`)
    /// increment it directly and never send this.
    OutboundAttempt {
        outcome: OutboundAttemptOutcome,
    },
    /// A call's RTCP-derived or receive-side media quality, for one
    /// `source` (specs/046-rtcp-reporting FR-008a). Sent once per source
    /// at call teardown, never per-report — the label set stays a closed
    /// enum with no per-call value (`source` only), per this type's own
    /// cardinality rule stated above.
    MediaQuality {
        source: QualitySource,
        loss_percent: f64,
        jitter_seconds: f64,
        /// `None` when not derivable for this source (e.g. `Local` never
        /// has one — round-trip time is inherently a property of what the
        /// *far end* reported about us).
        round_trip_seconds: Option<f64>,
    },
    /// A call proceeded with no RTCP endpoint obtainable at all (tier 3 of
    /// `ims::rtcp::bind_rtp_and_rtcp`) — media still carried the call
    /// (specs/046-rtcp-reporting SC-006), but this is the visibility
    /// FR-017a requires so that shortfall doesn't read as a healthy,
    /// silently RTCP-less bridge.
    RtcpUnavailable,
    /// Supervisor only: one steady-state recovery of this line, sent once per
    /// `SteadyOutcome::Recovered` (specs/096). An event rather than a running
    /// total in `AgentState` so a supervisor restart cannot rewind the count,
    /// and so a route that keeps vanishing -- repaired every tick, never
    /// counted as flapping -- shows up somewhere other than the logs.
    TunnelRecovered {
        reason: TunnelRecoveryReason,
        /// The P-CSCFs the agent had given up on, for a
        /// `PcscfPoolExhausted` recovery (specs/097): the daemon's alert names
        /// them. Empty, and absent on the wire, for every other reason, so
        /// those keep the shape spec 096 shipped.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pcscfs: Vec<String>,
        /// When the supervisor issued a `PcscfPoolExhausted` rebuild, in unix
        /// seconds; `None` for every other reason. Carried rather than taken
        /// from the report's arrival, because a report can queue behind an
        /// unreachable daemon and land after the replacement agent's own: the
        /// daemon ends the episode on a registration from an agent it first
        /// saw *after* this time (specs/097, Greptile PR #137).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rebuilt_at_unix: Option<u64>,
        /// The verdict on a dead-peer `IkeSaLost` (specs/098); `None`, and
        /// absent on the wire, for every other recovery.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dead_peer: Option<DeadPeerVerdict>,
    },
    /// Supervisor only: the underlay guard put this line on hold (specs/100).
    UplinkHoldStarted {
        cause: UplinkHoldCause,
    },
    /// Supervisor only: a hold ended, after `held_secs` (specs/100).
    UplinkHoldEnded {
        cause: UplinkHoldCause,
        held_secs: u64,
        ended_by: HoldEndedBy,
    },
}

/// Whose view of the call [`ObservedEvent::MediaQuality`] describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualitySource {
    /// This bridge's own measurement of what it received from the carrier.
    Local,
    /// What the carrier's own RTCP reports said it received from us.
    Remote,
}

/// Mirrors `sip::outbound::OutboundOutcome`'s label set exactly
/// (`gsm-sip-bridge/src/sip/outbound.rs`) without depending on it — this
/// module stays a plain wire-protocol type, the same layering
/// `CallStatus`/`SmsOutcome` already keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundAttemptOutcome {
    Placed,
    RefusedNoIdleLine,
    RefusedInvalidDestination,
    RefusedNetworkFailure,
    Unanswered,
    /// The originating caller hung up before the call connected — distinct
    /// from `Unanswered` ("the destination never picked up") and from
    /// `RefusedNetworkFailure` (specs/029-interruptible-origination-wait,
    /// FR-018). Only reachable once the origination wait is interruptible.
    CallerAbandoned,
}

impl OutboundAttemptOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutboundAttemptOutcome::Placed => "placed",
            OutboundAttemptOutcome::RefusedNoIdleLine => "refused_no_idle_line",
            OutboundAttemptOutcome::RefusedInvalidDestination => "refused_invalid_destination",
            OutboundAttemptOutcome::RefusedNetworkFailure => "refused_network_failure",
            OutboundAttemptOutcome::Unanswered => "unanswered",
            OutboundAttemptOutcome::CallerAbandoned => "caller_abandoned",
        }
    }

    /// The call-history status an attempt is filed under
    /// (specs/122-management-webui, data-model.md): a destination that never
    /// picked up, or a caller who gave up, is `missed`; a refusal is
    /// `failed`. `Placed` is `None`: that call is recorded as `answered`,
    /// with its duration, only once it ends.
    pub fn history_status(&self) -> Option<CallStatus> {
        match self {
            OutboundAttemptOutcome::Placed => None,
            OutboundAttemptOutcome::Unanswered | OutboundAttemptOutcome::CallerAbandoned => {
                Some(CallStatus::Missed)
            }
            OutboundAttemptOutcome::RefusedNoIdleLine
            | OutboundAttemptOutcome::RefusedInvalidDestination
            | OutboundAttemptOutcome::RefusedNetworkFailure => Some(CallStatus::Failed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Answered,
    Missed,
    Failed,
}

impl CallStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CallStatus::Answered => "answered",
            CallStatus::Missed => "missed",
            CallStatus::Failed => "failed",
        }
    }
}

/// Reused for both `PbxLegCompleted`'s outcome (`success`/`failed`, matching
/// `modules::mod`'s existing `SIP_CALLS_TOTAL` status values) and
/// `SmsForwarded`'s outcome (`sent`/`failed`, matching the existing
/// `SMS_FORWARDED_TOTAL` values) — same two-value shape, different label
/// vocabulary per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmsOutcome {
    Sent,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeFailureReason {
    BridgeSetupFailed,
    RingTimeout,
    CallerCancelled,
    /// The telephone side declined the call (`486`/`600`/`603`) — and only
    /// that; a no-answer is `RingTimeout`, a broken PBX `PbxFailed`.
    PbxDeclined,
    /// The telephone-side leg ended before answer for a reason other than a
    /// decline or a no-answer — unknown extension, PBX error, transport
    /// failure, or no status at all (specs/084).
    PbxFailed,
    AgentUnreachable,
    /// The call connected but audio did not flow both ways (FR-017) — a bridged
    /// call that carried one-way or no audio is a failure, not a success.
    OneWayAudio,
}

impl BridgeFailureReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            BridgeFailureReason::BridgeSetupFailed => "bridge_setup_failed",
            BridgeFailureReason::RingTimeout => "ring_timeout",
            BridgeFailureReason::CallerCancelled => "caller_cancelled",
            BridgeFailureReason::PbxDeclined => "pbx_declined",
            BridgeFailureReason::PbxFailed => "pbx_failed",
            BridgeFailureReason::AgentUnreachable => "agent_unreachable",
            BridgeFailureReason::OneWayAudio => "one_way_audio",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationStatus {
    Success,
    AuthFailed,
    Rejected,
    Timeout,
}

impl RegistrationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RegistrationStatus::Success => "success",
            RegistrationStatus::AuthFailed => "auth_failed",
            RegistrationStatus::Rejected => "rejected",
            RegistrationStatus::Timeout => "timeout",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ControlResp {
    Ok,
    OkMode { mode: String },
    OkSlots { slots: Vec<SlotInfo> },
    Err { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotInfo {
    pub slot: u32,
    pub state: String,
    pub phone: String,
    pub network: String,
    /// The line id calls and SMS are recorded under (specs/122-management-
    /// webui). Defaulted so a reply from an older daemon still decodes.
    #[serde(default)]
    pub module_id: String,
    /// Whether a call is in progress on this slot.
    #[serde(default)]
    pub has_active_call: bool,
}

pub fn read_cmd<R: BufRead>(reader: &mut R) -> Result<ControlCmd, String> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;
    serde_json::from_str(line.trim()).map_err(|e| format!("parse error: {e}"))
}

pub fn write_resp<W: Write>(writer: &mut W, resp: &ControlResp) -> Result<(), String> {
    let mut json = serde_json::to_string(resp).map_err(|e| format!("serialize error: {e}"))?;
    json.push('\n');
    writer
        .write_all(json.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    Ok(())
}

impl ControlResp {
    pub fn ok() -> Self {
        ControlResp::Ok
    }

    pub fn ok_mode(mode: NetworkMode) -> Self {
        ControlResp::OkMode {
            mode: mode.to_string(),
        }
    }

    pub fn ok_slots(slots: Vec<SlotInfo>) -> Self {
        ControlResp::OkSlots { slots }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        ControlResp::Err { error: msg.into() }
    }
}

impl Serialize for ControlResp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            ControlResp::Ok => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("ok", &true)?;
                map.end()
            }
            ControlResp::OkMode { mode } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("mode", mode)?;
                map.end()
            }
            ControlResp::OkSlots { slots } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &true)?;
                map.serialize_entry("slots", slots)?;
                map.end()
            }
            ControlResp::Err { error } => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("ok", &false)?;
                map.serialize_entry("error", error)?;
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn supervisor_report_round_trips_child_sa_up() {
        let report = AgentReport {
            agent: AgentKind::Supervisor,
            module_id: "m".into(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                child_sa_up: Some(false),
                ..Default::default()
            },
            events: vec![],
            dropped: 0,
            details: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"agent\":\"supervisor\""));
        assert!(
            !json.contains("details"),
            "an absent details must not appear on the wire"
        );
        assert!(json.contains("\"child_sa_up\":false"));
        let back: AgentReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent, AgentKind::Supervisor);
        assert_eq!(back.state.child_sa_up, Some(false));
    }

    #[test]
    fn outbound_outcomes_map_to_history_statuses() {
        use OutboundAttemptOutcome as O;
        assert_eq!(O::Placed.history_status(), None);
        assert_eq!(O::Unanswered.history_status(), Some(CallStatus::Missed));
        assert_eq!(
            O::CallerAbandoned.history_status(),
            Some(CallStatus::Missed)
        );
        for failed in [
            O::RefusedNoIdleLine,
            O::RefusedInvalidDestination,
            O::RefusedNetworkFailure,
        ] {
            assert_eq!(failed.history_status(), Some(CallStatus::Failed));
        }
    }

    #[test]
    fn a_report_without_details_still_decodes() {
        let json = r#"{"agent":"ims","module_id":"m","epoch":1,"seq":2,"state":{}}"#;
        let report: AgentReport = serde_json::from_str(json).unwrap();
        assert!(report.details.is_none());
    }

    #[test]
    fn details_round_trip() {
        let details = AgentDetails {
            registration_state: Some(RegistrationStateWire::Renewing),
            registered_at: Some(1_700_000_000),
            plmn: Some("405-857".into()),
            signal: Some(SignalReadingWire {
                at_unix: 1_700_000_100,
                rssi_dbm: Some(-71),
                rat: Some("LTE".into()),
                rsrp_dbm: Some(-95),
                rsrq_db: Some(-10),
                sinr_db: None,
            }),
            active_call_details: None,
        }
        .with_active_calls(vec![ActiveCallInfoWire {
            remote: "+919000000001".into(),
            direction: CallDirectionWire::Outbound,
            started_at_unix: 1_700_000_050,
            answered: true,
            loss_percent: Some(0.5),
            jitter_ms: None,
            rtt_ms: Some(80.0),
        }]);
        let report = AgentReport {
            agent: AgentKind::Ims,
            module_id: "m".into(),
            epoch: 1,
            seq: 1,
            state: AgentState::default(),
            events: vec![],
            dropped: 0,
            details: Some(details.clone()),
        };
        let back: AgentReport =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(back.details, Some(details));
        assert_eq!(
            back.details
                .unwrap()
                .registration_state
                .unwrap()
                .to_string(),
            "renewing"
        );
    }

    #[test]
    fn active_calls_are_truncated_to_the_cap() {
        let call = |n: u64| ActiveCallInfoWire {
            remote: format!("+9190000000{n:02}"),
            direction: CallDirectionWire::Inbound,
            started_at_unix: n,
            answered: false,
            loss_percent: None,
            jitter_ms: None,
            rtt_ms: None,
        };
        let details = AgentDetails::default().with_active_calls((0..9).map(call).collect());
        assert_eq!(
            details.active_call_details.unwrap().len(),
            MAX_ACTIVE_CALL_DETAILS
        );
    }

    #[test]
    fn registration_state_wire_matches_the_cli_wording() {
        use crate::ims::RegistrationState as S;
        for s in [
            S::Unregistered,
            S::Registering,
            S::Registered,
            S::Renewing,
            S::Failed,
        ] {
            assert_eq!(
                RegistrationStateWire::from(s).to_string(),
                crate::volte::registration::state_label(s)
            );
        }
    }

    #[test]
    fn ike_sa_loss_cause_serde_names_and_display() {
        use IkeSaLossCause::*;
        for (c, wire, text) in [
            (CarrierDelete, "\"carrier_delete\"", "carrier DELETE"),
            (DeadPeer, "\"dead_peer\"", "dead-peer timeout"),
            (Unknown, "\"unknown\"", "unknown"),
        ] {
            assert_eq!(serde_json::to_string(&c).unwrap(), wire);
            assert_eq!(serde_json::from_str::<IkeSaLossCause>(wire).unwrap(), c);
            assert_eq!(c.to_string(), text);
        }
    }

    #[test]
    fn a_stuck_half_open_ike_sa_has_its_own_wire_label() {
        let event = ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::IkeSaNegotiationStuck,
            pcscfs: Vec::new(),
            rebuilt_at_unix: None,
            dead_peer: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"ike_sa_negotiation_stuck\""), "{json}");
        assert_eq!(serde_json::from_str::<ObservedEvent>(&json).unwrap(), event);
    }

    #[test]
    fn tunnel_recovery_reasons_round_trip_under_their_labels() {
        for reason in TunnelRecoveryReason::ALL {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{}\"", reason.as_str()));
            let back: TunnelRecoveryReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn tunnel_recovered_event_wire_shape() {
        let event = ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::DefaultRouteMissing,
            pcscfs: Vec::new(),
            rebuilt_at_unix: None,
            dead_peer: None,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"tunnel_recovered","reason":"default_route_missing"}"#
        );
    }

    #[test]
    fn a_dead_peer_recovery_carries_its_verdict_and_old_payloads_still_parse() {
        let json = r#"{"event":"tunnel_recovered","reason":"ike_sa_lost","dead_peer":"false"}"#;
        let event = ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::IkeSaLost,
            pcscfs: Vec::new(),
            rebuilt_at_unix: None,
            dead_peer: Some(DeadPeerVerdict::False),
        };
        assert_eq!(serde_json::to_string(&event).unwrap(), json);
        assert_eq!(serde_json::from_str::<ObservedEvent>(json).unwrap(), event);
        let old: ObservedEvent =
            serde_json::from_str(r#"{"event":"tunnel_recovered","reason":"ike_sa_lost"}"#).unwrap();
        assert!(matches!(
            old,
            ObservedEvent::TunnelRecovered {
                dead_peer: None,
                ..
            }
        ));
        for v in DeadPeerVerdict::ALL {
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                format!("\"{}\"", v.as_str())
            );
        }
    }

    #[test]
    fn uplink_states_causes_and_end_reasons_serialize_under_their_labels() {
        for state in [
            UplinkState::Up,
            UplinkState::NoRoute,
            UplinkState::NoResponse,
        ] {
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{}\"", state.as_str())
            );
        }
        for cause in UplinkHoldCause::ALL {
            assert_eq!(
                serde_json::to_string(&cause).unwrap(),
                format!("\"{}\"", cause.as_str())
            );
            assert_eq!(UplinkState::from(cause).as_str(), cause.as_str());
        }
        for ended in [
            HoldEndedBy::Restored,
            HoldEndedBy::MaxHold,
            HoldEndedBy::Shutdown,
            HoldEndedBy::CauseChanged,
        ] {
            assert_eq!(
                serde_json::to_string(&ended).unwrap(),
                format!("\"{}\"", ended.as_str())
            );
        }
        assert_eq!(DeadPeerVerdict::Uplink.as_str(), "uplink");
    }

    #[test]
    fn uplink_state_and_hold_events_round_trip_and_old_payloads_still_parse() {
        let state = AgentState {
            uplink: Some(UplinkState::NoRoute),
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, r#"{"uplink":"no_route"}"#);
        assert_eq!(
            serde_json::from_str::<AgentState>(&json).unwrap().uplink,
            Some(UplinkState::NoRoute)
        );
        // Without the field (an older supervisor, or the guard off) it is absent.
        assert_eq!(serde_json::to_string(&AgentState::default()).unwrap(), "{}");
        assert_eq!(
            serde_json::from_str::<AgentState>(r#"{"child_sa_up":true}"#)
                .unwrap()
                .uplink,
            None
        );

        let started = ObservedEvent::UplinkHoldStarted {
            cause: UplinkHoldCause::NoRoute,
        };
        let json = serde_json::to_string(&started).unwrap();
        assert_eq!(
            json,
            r#"{"event":"uplink_hold_started","cause":"no_route"}"#
        );
        assert_eq!(
            serde_json::from_str::<ObservedEvent>(&json).unwrap(),
            started
        );

        let ended = ObservedEvent::UplinkHoldEnded {
            cause: UplinkHoldCause::NoResponse,
            held_secs: 312,
            ended_by: HoldEndedBy::Restored,
        };
        let json = serde_json::to_string(&ended).unwrap();
        assert_eq!(
            json,
            r#"{"event":"uplink_hold_ended","cause":"no_response","held_secs":312,"ended_by":"restored"}"#
        );
        assert_eq!(serde_json::from_str::<ObservedEvent>(&json).unwrap(), ended);
    }

    #[test]
    fn a_dead_peer_loss_attributed_to_the_uplink_round_trips() {
        let event = ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::IkeSaLost,
            pcscfs: Vec::new(),
            rebuilt_at_unix: None,
            dead_peer: Some(DeadPeerVerdict::Uplink),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""dead_peer":"uplink""#), "{json}");
        assert_eq!(serde_json::from_str::<ObservedEvent>(&json).unwrap(), event);
    }

    #[test]
    fn a_pool_exhausted_recovery_carries_its_pcscfs() {
        let json = r#"{"event":"tunnel_recovered","reason":"pcscf_pool_exhausted","pcscfs":["10.0.0.11"],"rebuilt_at_unix":1790000000}"#;
        let event = ObservedEvent::TunnelRecovered {
            reason: TunnelRecoveryReason::PcscfPoolExhausted,
            pcscfs: vec!["10.0.0.11".into()],
            rebuilt_at_unix: Some(1_790_000_000),
            dead_peer: None,
        };
        assert_eq!(serde_json::to_string(&event).unwrap(), json);
        let back: ObservedEvent = serde_json::from_str(json).unwrap();
        assert_eq!(back, event);
        // A spec-096 sender's event, with no `pcscfs`, still decodes.
        let old: ObservedEvent =
            serde_json::from_str(r#"{"event":"tunnel_recovered","reason":"pcscf_unreachable"}"#)
                .unwrap();
        assert_eq!(
            old,
            ObservedEvent::TunnelRecovered {
                reason: TunnelRecoveryReason::PcscfUnreachable,
                pcscfs: Vec::new(),
                rebuilt_at_unix: None,
                dead_peer: None,
            }
        );
    }

    #[test]
    fn flapping_and_loss_cause_round_trip_and_are_optional() {
        let report = AgentReport {
            agent: AgentKind::Supervisor,
            module_id: "m".into(),
            epoch: 1,
            seq: 1,
            state: AgentState {
                tunnel_flapping: Some(true),
                ike_sa_loss_cause: Some(IkeSaLossCause::CarrierDelete),
                ..Default::default()
            },
            events: vec![],
            dropped: 0,
            details: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"tunnel_flapping\":true"));
        assert!(json.contains("\"ike_sa_loss_cause\":\"carrier_delete\""));
        let back: AgentReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state.tunnel_flapping, Some(true));
        assert_eq!(
            back.state.ike_sa_loss_cause,
            Some(IkeSaLossCause::CarrierDelete)
        );
        let empty: AgentState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.tunnel_flapping, None);
        assert_eq!(empty.ike_sa_loss_cause, None);
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("tunnel_flapping") && !json.contains("ike_sa_loss_cause"));
    }

    #[test]
    fn child_sa_up_is_optional_on_the_wire() {
        let state: AgentState = serde_json::from_str("{}").unwrap();
        assert_eq!(state.child_sa_up, None);
        assert!(!serde_json::to_string(&state)
            .unwrap()
            .contains("child_sa_up"));
    }

    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_cmd_card_restart_roundtrip() {
        let json = r#"{"cmd":"card_restart","slot":0}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(
            cmd,
            ControlCmd::CardRestart {
                slot: 0,
                ref mode
            } if mode == "full"
        ));
    }

    #[test]
    fn test_cmd_card_restart_with_explicit_radio_mode() {
        let json = r#"{"cmd":"card_restart","slot":2,"mode":"radio"}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(
            cmd,
            ControlCmd::CardRestart {
                slot: 2,
                ref mode
            } if mode == "radio"
        ));
    }

    #[test]
    fn test_cmd_set_mode_roundtrip() {
        let json = r#"{"cmd":"set_mode","slot":1,"mode":"4g"}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::SetMode { slot: 1, .. }));
    }

    #[test]
    fn test_cmd_get_mode_roundtrip() {
        let json = r#"{"cmd":"get_mode","slot":2}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::GetMode { slot: 2 }));
    }

    #[test]
    fn test_cmd_list_slots_roundtrip() {
        let json = r#"{"cmd":"list_slots"}"#;
        let cmd: ControlCmd = serde_json::from_str(json).unwrap();
        assert!(matches!(cmd, ControlCmd::ListSlots));
    }

    #[test]
    fn test_resp_ok_serializes() {
        let resp = ControlResp::ok();
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true}"#);
    }

    #[test]
    fn test_resp_ok_mode_serializes() {
        let resp = ControlResp::OkMode {
            mode: "4g".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":true,"mode":"4g"}"#);
    }

    #[test]
    fn test_resp_err_serializes() {
        let resp = ControlResp::err("slot 5 not found");
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"ok":false,"error":"slot 5 not found"}"#);
    }

    #[test]
    fn test_read_cmd_and_write_resp() {
        let input = b"{ \"cmd\": \"list_slots\" }\n";
        let mut reader = Cursor::new(input.as_slice());
        let cmd = read_cmd(&mut reader).unwrap();
        assert!(matches!(cmd, ControlCmd::ListSlots));

        let mut output = Vec::new();
        write_resp(&mut output, &ControlResp::ok()).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "{\"ok\":true}\n");
    }
}
