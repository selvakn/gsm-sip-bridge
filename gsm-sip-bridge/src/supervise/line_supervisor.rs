//! One per-line VoWiFi tunnel supervisor, generic over the tunnel engine
//! (specs/021-entrypoint-supervise-rust Phase 3) — collapses the three
//! duplicated bash loops (strongswan establish-time, strongswan steady-state,
//! swu establish+steady-state) into one state machine (FR-005). Every
//! transition and its recovery action is a 1:1 port of the original script's
//! control flow, not a redesign: see the per-branch comments below for the
//! exact bash this replaces.

use super::engines::LogMark;
use super::runner::CommandRunner;
pub use crate::control::protocol::DeadPeerVerdict;
use crate::control::protocol::{IkeSaLossCause, UplinkHoldCause};
use crate::ims::pcscf_files::PoolState;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Mirrors the current establish-time loop's `attempt`/`stuck_without_pcscf`
/// locals and the steady-state loop's tracked P-CSCF.
#[derive(Debug, Clone, PartialEq)]
pub enum LineState {
    Establishing {
        attempt: u32,
        stuck_without_pcscf: bool,
    },
    Up {
        pcscf: String,
    },
    /// A steady-state problem was detected this tick; the caller (the
    /// per-line supervisor thread) has already issued the recovery action by
    /// the time `tick` returns this — `Restarting` is a one-tick transient
    /// the next `tick` moves on from, mirroring the bash loop's `continue`.
    Degraded {
        reason: DegradeReason,
    },
    Restarting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason {
    ProcessDied,
    ViciBroken,
    TunVanished,
    /// The tunnel interface is missing *and* could not be recreated, so this
    /// line has no data path and cannot be given one right now — distinct
    /// from `TunVanished`, which is the same symptom with a recreation that
    /// worked. Nothing downstream of the interface is worth disturbing until
    /// it exists: see the TunVanished branch of [`tick_steady_state`].
    TunUnavailable,
    /// The CHILD_SA was lost under a live (listed) IKE_SA; re-initiated in
    /// place, the agent untouched.
    ChildSaMissing,
    /// The IKE_SA itself is gone: a carrier DELETE or a dead-peer clear (#106).
    /// A new IKE_SA brings a new VIP and P-CSCF pool, so the agent always
    /// restarts (spec 087, clarification Q1).
    IkeSaLost,
    /// The CHILD_SA stayed missing across consecutive ticks even though it was
    /// re-initiated (#107): the ePDG ignored CREATE_CHILD_SA inside the
    /// surviving IKE_SA, and only a fresh IKE_SA recovers. The agent must
    /// re-register because the new IKE_SA brings a new VIP/P-CSCF pool.
    ChildSaStuck,
    /// The same CHILD_SA outlived the window a natural rekey replaces it in,
    /// so its rekey has failed, and it was rebuilt before its hard expiry
    /// rather than after (#127, specs/093). Same remedy as `ChildSaStuck`;
    /// the difference is that the old CHILD_SA was still carrying traffic.
    ChildSaRekeyStuck,
    PcscfChanged,
    /// The P-CSCF became unreachable from inside this line's namespace for
    /// several consecutive ticks, while every structural check still passed
    /// (specs/039-at-stall-watchdog).
    ///
    /// This is the 2026-08-17 outage: after the carrier moved the P-CSCF, the
    /// tunnel re-established -- IKE SA up, CHILD_SA installed, interface
    /// present -- but the `default dev tun23-0` route was never reinstalled.
    /// So there was no data path, the inbound SA carried 0 bytes, and the
    /// agent got `ENETUNREACH` and exited every 6 seconds for **8 hours**.
    /// Every structural check passed the whole time, because none of them
    /// looks at whether anything can actually be reached.
    PcscfUnreachable,
    /// The line's netns had no default route through its tunnel, and one has
    /// been reinstated (specs/039-at-stall-watchdog).
    ///
    /// The 2026-08-19 outage: a 2-minute WAN blip (a scheduled router reboot)
    /// tore the CHILD_SA down, charon's `down-client` removed the carrier
    /// address, and the kernel deleted the interface's default route along with
    /// it. The reconnect restored the address but nothing restored the route, so
    /// every SIP connect got `ENETUNREACH` for six hours while
    /// `swanctl --list-sas` reported a healthy tunnel throughout.
    ///
    /// Distinct from [`Self::PcscfUnreachable`] because the remedy is completely
    /// different — one `ip route replace`, no renegotiation — and because a
    /// rebuild provably cannot fix it. The line's IMS agent is still restarted
    /// ([`recovery_restarts_agent`]), so a call in progress is dropped.
    DefaultRouteMissing,
    /// The line's IMS agent has marked every P-CSCF in the pool unavailable
    /// since the tunnel was last negotiated, so it has nowhere left to fail
    /// over to (specs/097, #120 finding 4).
    ///
    /// The agent finds a dead P-CSCF on its own -- three unanswered Gm
    /// keepalives, a REGISTER that times out -- and moves to a sibling. On a
    /// carrier that advertises one P-CSCF (Vodafone IN) there is no sibling,
    /// and only a new negotiation brings a new address. Without this, a line
    /// whose only P-CSCF died stayed down until someone restarted it: in the
    /// #120 probes-off soak it never recovered. The same remedy as
    /// [`Self::PcscfUnreachable`], decided from the agent's own verdict instead
    /// of a TCP probe of the unprotected port.
    PcscfPoolExhausted,
    /// A half-open IKE_SA showed no IKE activity for [`HALF_OPEN_STUCK_AFTER`],
    /// so charon is no longer working on it; terminated and re-initiated
    /// (specs/100 US3). The new IKE_SA brings a new VIP and P-CSCF pool, so
    /// the agent restarts, as for [`Self::ChildSaStuck`].
    IkeSaNegotiationStuck,
}

/// The wire label for a recovery (specs/096). Exhaustive on purpose, with no
/// wildcard arm: a new reason does not compile until it has a label.
impl From<DegradeReason> for crate::control::protocol::TunnelRecoveryReason {
    fn from(reason: DegradeReason) -> Self {
        match reason {
            DegradeReason::ProcessDied => Self::ProcessDied,
            DegradeReason::ViciBroken => Self::ViciBroken,
            DegradeReason::TunVanished => Self::TunVanished,
            DegradeReason::TunUnavailable => Self::TunUnavailable,
            DegradeReason::ChildSaMissing => Self::ChildSaMissing,
            DegradeReason::IkeSaLost => Self::IkeSaLost,
            DegradeReason::ChildSaStuck => Self::ChildSaStuck,
            DegradeReason::ChildSaRekeyStuck => Self::ChildSaRekeyStuck,
            DegradeReason::PcscfChanged => Self::PcscfChanged,
            DegradeReason::PcscfUnreachable => Self::PcscfUnreachable,
            DegradeReason::DefaultRouteMissing => Self::DefaultRouteMissing,
            DegradeReason::PcscfPoolExhausted => Self::PcscfPoolExhausted,
            DegradeReason::IkeSaNegotiationStuck => Self::IkeSaNegotiationStuck,
        }
    }
}

/// Steady-state-only health, beyond "is the process alive" and "is a P-CSCF
/// change pending" (checked separately) — the three strongswan-specific
/// checks (`tun_iface` presence, `swanctl --list-sas`'s exit status, the
/// CHILD_SA line's presence under the IKE_SA) collapse to `Ok` for the swu engine,
/// which the original script's own comment says has "no re-initiate-in-place
/// concept."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteadyStateHealth {
    /// Structurally healthy, with the newest usable CHILD_SA's lifetime.
    /// `None` when the engine reports no CHILD_SA lifetimes (swu).
    Ok(Option<ChildSaLifetime>),
    ViciBroken,
    TunVanished,
    /// No IKE_SA is listed for this connection at all (specs/087).
    IkeSaMissing,
    /// An IKE_SA is listed (any state, `CONNECTING` included) but has no
    /// usable CHILD_SA.
    ChildSaMissing,
    /// The IKE_SA `#ike_id` is half-open (`CONNECTING`): charon is still
    /// negotiating it, which is not a lost CHILD_SA (specs/100 US3).
    IkeSaConnecting {
        ike_id: u32,
    },
}

/// What one steady-state tick saw of a line's newest usable CHILD_SA
/// (specs/093). The newest is the highest `#N`, so during a rekey's brief
/// overlap the replacement is judged, never the SA it is replacing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildSaLifetime {
    /// The `#N` on the CHILD_SA's `--list-sas` header (vici's `uniqueid`).
    /// It changes whenever the CHILD_SA is replaced, by a rekey or otherwise.
    pub unique_id: u32,
    /// Seconds until the hard expiry (`expires in Ns`); `None` when the
    /// listing carries no such field.
    pub expires_in: Option<i64>,
}

/// What one steady-state tick saw of inbound ESP across every CHILD_SA the
/// line's connection lists, a rekey's old and new SA included (specs/098 R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundEsp {
    /// The youngest inbound last-use age (`in … Ns ago`); `None` when no
    /// listed CHILD_SA has received anything.
    pub last_in_age: Option<Duration>,
    /// Inbound packets summed over those CHILD_SAs, for the log line only:
    /// a rekey resets them, so they are never compared across ticks.
    pub packets: u64,
    /// The tunnel interface's received-packet counter on the same tick. It
    /// counts only what was decrypted, survives a rekey, and stops when
    /// charon deletes the CHILD_SA, so comparing it with the loss tick's
    /// reading covers the time the snapshot cannot. `None` if unread.
    pub tun_rx_packets: Option<u64>,
}

/// The state column of a `swanctl --list-sas` IKE_SA header (specs/100).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IkeHeaderState {
    Established,
    /// An IKE_SA_INIT / IKE_AUTH still in flight: listed, but not usable.
    Connecting,
    Other(String),
}

/// What the charon log shows of the ePDG's side of one connection since a log
/// mark (specs/100 research R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IkeEvidence {
    /// A `parsed … request|response` line for the connection: the ePDG said
    /// something, whether it answered us or sent its own request.
    pub parsed_from_peer: bool,
    /// Retransmits of an outstanding request since the last parsed message.
    pub unanswered_retransmits: u32,
}

/// A fingerprint of the IKE traffic charon has logged for one connection:
/// how many such lines and the last of them (specs/100). Two readings that
/// differ mean charon sent, retransmitted or parsed something in between.
///
/// Compared, never read as a time: charon stamps its log in the container's
/// local time, which is not UTC on every deployment (the Jio Pi's is IST), and
/// the supervisor only needs to know *whether* charon has been busy since its
/// last look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IkeActivity {
    pub lines: usize,
    pub last: String,
}

impl IkeEvidence {
    /// The value for an engine that cannot tell: life seen, nothing unanswered.
    pub const LIFE: Self = Self {
        parsed_from_peer: true,
        unanswered_retransmits: 0,
    };
}

/// How long charon retransmits an unanswered request before giving up on the
/// peer: sent once and retransmitted `retransmit_tries` (5) times at
/// `retransmit_timeout · retransmit_base^k` = 4s · 1.8^k, so
/// 4 + 7.2 + 13.0 + 23.3 + 42.0 + 75.6 ≈ 165s. charon's defaults, which
/// `charon-extra.conf` leaves alone; tuning those means changing this too.
pub const DPD_GIVE_UP_WINDOW: Duration = Duration::from_secs(165);

/// A dead-peer verdict and the evidence it rests on, for the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadPeerClassification {
    pub verdict: DeadPeerVerdict,
    /// How old the snapshot was when the loss was seen.
    pub snapshot_age: Option<Duration>,
    /// How long before the loss was seen the last inbound packet arrived.
    pub last_in_before_loss: Option<Duration>,
    pub packets: Option<u64>,
    /// Packets the tunnel interface received between the snapshot and the
    /// loss; `None` when either reading is missing or the counter went back.
    pub rx_since_snapshot: Option<u64>,
}

/// Classifies a dead-peer loss seen at `now` from the last structurally
/// healthy tick's inbound snapshot (specs/098 R4, contracts/dead-peer.md).
///
/// charon gave up somewhere after the snapshot `S` and at or before `now`
/// (`T`), so its first unanswered request went out in `(S − G, T − G]`, with
/// `G` = [`DPD_GIVE_UP_WINDOW`]. An inbound packet at `L` after `T − G` came
/// while charon was retransmitting (false); one at or before `S − G` came
/// before the request could have gone out (real). In between, or with no
/// inbound use on any listed SA, cannot be told.
/// For a really dead peer DPD itself needed `dpd_delay` of silence first, so
/// at the default `L` sits well below `S − G`.
///
/// The snapshot says nothing about `(S, T]`, and a path that recovers there
/// still loses the tunnel, since charon keeps retransmitting regardless. So
/// the tunnel interface's counter, read at `S` and again at `T`, settles it:
/// a packet decrypted after `S` arrived after `T − G` whenever the snapshot is
/// younger than `G` (false), and `real` needs the counter to show none.
pub fn classify_dead_peer(
    snapshot: Option<(Instant, InboundEsp)>,
    now: Instant,
    tun_rx_now: Option<u64>,
) -> DeadPeerClassification {
    let Some((taken, esp)) = snapshot else {
        return DeadPeerClassification {
            verdict: DeadPeerVerdict::Unknown,
            snapshot_age: None,
            last_in_before_loss: None,
            packets: None,
            rx_since_snapshot: None,
        };
    };
    let snapshot_age = now.saturating_duration_since(taken);
    let last_in_before_loss = esp.last_in_age.map(|age| snapshot_age + age);
    let rx_since_snapshot = esp
        .tun_rx_packets
        .zip(tun_rx_now)
        .and_then(|(then, now)| now.checked_sub(then));
    let verdict = match last_in_before_loss {
        // `L > T − G`  ⇔  `T − L < G`
        Some(before) if before < DPD_GIVE_UP_WINDOW => DeadPeerVerdict::False,
        // Decrypted after `S`, and `S > T − G` while the snapshot is younger
        // than `G`.
        _ if snapshot_age < DPD_GIVE_UP_WINDOW && rx_since_snapshot.is_some_and(|n| n > 0) => {
            DeadPeerVerdict::False
        }
        // No listed SA has received anything, which a fresh rekey's SA also
        // shows. A really dead peer cannot have completed that rekey, so its
        // SA still carries a last-use age; this is not evidence either way.
        None => DeadPeerVerdict::Unknown,
        // `L ≤ S − G`  ⇔  `S − L ≥ G`, and `S − L` is the snapshot's own age;
        // and nothing arrived in `(S, T]`.
        Some(_)
            if esp.last_in_age.is_some_and(|age| age >= DPD_GIVE_UP_WINDOW)
                && rx_since_snapshot == Some(0) =>
        {
            DeadPeerVerdict::Real
        }
        Some(_) => DeadPeerVerdict::Unknown,
    };
    DeadPeerClassification {
        verdict,
        snapshot_age: Some(snapshot_age),
        last_in_before_loss,
        packets: Some(esp.packets),
        rx_since_snapshot,
    }
}

/// Matches the establish-time loop's `sleep 2` between polls, both engines.
pub const ESTABLISH_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Matches the strongswan establish-time loop's `$((attempt % 15))`
/// re-initiate cadence (2s * 15 = 30s between re-initiate attempts).
pub const STRONGSWAN_REINITIATE_EVERY: u32 = 15;
/// Matches the swu establish-time loop's `seq 1 90` bound (90 * 2s = 180s).
pub const SWU_MAX_ESTABLISH_ATTEMPTS: u32 = 90;
/// Matches the strongswan steady-state loop's `sleep 30` cadence.
pub const STEADY_STATE_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// The swu steady-state loop's own, much faster cadence.
///
/// Named rather than inline in `orchestrate` because the reachability window
/// below is derived from it: the two engines poll six times apart, so anything
/// expressed in ticks means two different things depending on which loop is
/// running it.
pub const SWU_STEADY_STATE_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long the P-CSCF must stay unreachable before the tunnel is rebuilt.
///
/// A *duration*, not a tick count. This was three ticks, which gives ~90s on the
/// strongswan loop but only ~15s on the swu loop — so on swu a routine 15s
/// bearer blip tore down a working tunnel and restarted the agent, while the
/// doc comment and the real-time bound test both described the 90s figure.
///
/// 90s because a genuine loss of the data path should be repaired in about a
/// minute and a half rather than never, and because a single failed TCP connect
/// is a routine thing on a mobile bearer — one dropped packet during a rekey
/// must not be enough.
pub const PCSCF_UNREACHABLE_WINDOW: Duration = Duration::from_secs(90);

/// However fast a loop polls, never act on fewer than this many consecutive
/// failures. A time window alone would let an arbitrarily slow poll act on one
/// sample, and one sample is exactly what is too noisy to trust.
pub const PCSCF_UNREACHABLE_MIN_STRIKES: u32 = 2;

/// Consecutive unreachable ticks that cover [`PCSCF_UNREACHABLE_WINDOW`] at
/// `poll_interval`.
pub fn pcscf_unreachable_strikes(poll_interval: Duration) -> u32 {
    let per_tick = poll_interval.as_secs().max(1);
    let ticks = PCSCF_UNREACHABLE_WINDOW.as_secs().div_ceil(per_tick);
    u32::try_from(ticks)
        .unwrap_or(u32::MAX)
        .max(PCSCF_UNREACHABLE_MIN_STRIKES)
}

/// SIP port probed on the P-CSCF -- the thing a line's data path has to be able
/// to reach for the line to be worth anything.
pub const PCSCF_SIP_PORT: u16 = 5060;

/// Engine-specific behavior the shared state machine drives. One
/// implementation per `[vowifi].tunnel_engine` value
/// (`StrongswanEngine`/`SwuEngine`, below) — this is the seam research.md R6
/// calls out as what collapses the two engines' duplicate loops into one
/// transition table without losing engine-specific behavior.
pub trait TunnelEngine {
    /// Is the tunnel established, per this engine's own log marker
    /// (`CHILD_SA.*established` for strongswan, `STATE CONNECTED` for swu)?
    fn is_tunnel_established(&self, runner: &dyn CommandRunner) -> bool;
    /// Latest P-CSCF address from wherever this engine's log records it.
    fn latest_pcscf(&self, runner: &dyn CommandRunner) -> Option<String>;
    /// Every P-CSCF the latest negotiation advertised, deduplicated, in log
    /// order (specs/091 R1). Published beside [`Self::latest_pcscf`]; never
    /// compared by the "P-CSCF changed" check.
    fn latest_pcscf_pool(&self, runner: &dyn CommandRunner) -> Vec<String>;
    /// Is this line's primary process (charon / the swu dialer) still alive?
    fn is_process_alive(&self, runner: &dyn CommandRunner) -> bool;
    /// Terminates the current negotiation without restarting the process
    /// (strongswan: a graceful `swanctl --terminate --ike <this line's
    /// connection>` waiting up to 5s for the ePDG's acknowledgement, then
    /// `--force` only if the IKE_SA is still listed, each bounded by an outer
    /// timeout;
    /// swu: no-op — no in-place terminate exists for this engine). Scoped to
    /// one connection: every line's connection lives in one shared charon, so
    /// a bare `ims` would tear down every line at once.
    fn terminate(&self, runner: &dyn CommandRunner);
    /// (Re)initiates a connection attempt without restarting the process
    /// (strongswan: `swanctl --initiate --child <this line's connection>`;
    /// swu: no-op, matching `terminate`). Scoped for the same reason.
    fn reinitiate(&self, runner: &dyn CommandRunner);
    /// Why this line's IKE_SA was lost, from the engine's own log. Only the
    /// strongswan engine can tell; the default is `Unknown` (specs/087).
    fn ike_sa_loss_cause(&self, _runner: &dyn CommandRunner) -> IkeSaLossCause {
        IkeSaLossCause::Unknown
    }
    /// Inbound ESP as the last [`Self::steady_state_health`] listed it, for
    /// classifying a later dead-peer loss (specs/098). Only the strongswan
    /// engine lists SAs; the default is `None`.
    fn inbound_esp(&self) -> Option<InboundEsp> {
        None
    }
    /// The tunnel interface's received-packet counter as the last
    /// [`Self::steady_state_health`] read it, on whatever tick that was; the
    /// loss tick's own reading, for [`classify_dead_peer`]. Only the
    /// strongswan engine reads it; the default is `None`.
    fn tun_rx_packets(&self) -> Option<u64> {
        None
    }
    /// The notify the ePDG last refused a CHILD_SA with in the current
    /// IKE_SA session, for the stuck-rekey log line only (specs/093 R4). Only
    /// the strongswan engine can tell; the default is `None`.
    fn child_sa_rejection(&self, _runner: &dyn CommandRunner) -> Option<String> {
        None
    }
    /// The ePDG address this line's tunnel runs to, for the underlay guard's
    /// route check (specs/100). `None` for an engine that has none (swu).
    fn epdg_addr(&self) -> Option<&str> {
        None
    }
    /// Does the host have a usable route to the ePDG? `true` when the engine
    /// cannot tell, so the guard never holds on no evidence (specs/100 R3).
    fn route_ok(&self, _runner: &dyn CommandRunner) -> bool {
        true
    }
    /// The IKE_SA header as the last [`Self::steady_state_health`] listed it,
    /// without listing again (specs/100).
    fn ike_header(&self) -> Option<(u32, IkeHeaderState)> {
        None
    }
    /// A fingerprint of the IKE traffic charon has logged for this connection:
    /// messages generated, parsed or retransmitted (specs/100 R6).
    fn ike_activity(&self, _runner: &dyn CommandRunner) -> Option<IkeActivity> {
        None
    }
    /// What the log shows of the ePDG since `mark` (specs/100 R4). The
    /// default reads as life seen, so an engine without a log never reports
    /// the path down.
    fn ike_evidence_since(&self, _runner: &dyn CommandRunner, _mark: LogMark) -> IkeEvidence {
        IkeEvidence::LIFE
    }
    /// A position in the engine's log, for [`Self::ike_evidence_since`].
    fn log_mark(&self, _runner: &dyn CommandRunner) -> LogMark {
        LogMark::default()
    }
    /// Queues one liveness check on the established IKE_SA (specs/100
    /// FR-004a), returning whether it was accepted. Does not wait for the
    /// answer.
    fn send_dpd(&self, _runner: &dyn CommandRunner) -> bool {
        false
    }
    /// What the steady-state tick's own log lines start with, naming this
    /// line and connection (`[supervise] line 0: ims0`).
    fn log_prefix(&self) -> String {
        "[supervise]".to_string()
    }
    /// Steady-state-only health beyond process-alive/P-CSCF-change.
    fn steady_state_health(&self, runner: &dyn CommandRunner) -> SteadyStateHealth;
    /// Recreates this line's tunnel interface after `SteadyStateHealth::
    /// TunVanished` — 1:1 port of the original script's own comment: "tun can
    /// vanish from the kernel entirely while swanctl still reports the
    /// CHILD_SA ESTABLISHED/INSTALLED (observed live, specs/012-strongswan-
    /// epdg) — recreate ... rather than trusting the desynced SA." Found
    /// missing live: an earlier version of this port detected TunVanished
    /// correctly but never recreated the interface before re-initiating,
    /// so every subsequent negotiation kept succeeding at the IKE/CHILD_SA
    /// level while silently failing to install a working data path — a
    /// fresh IKE_SA every ~30s (one steady-state tick), forever. No-op for
    /// swu, which has no equivalent pre-created interface concept.
    ///
    /// Returns whether the interface is present afterwards. `false` means no
    /// data path can exist for this line right now whatever else is done to
    /// it, so the caller skips the terminate/reinitiate that would otherwise
    /// follow — see the TunVanished branch of [`tick_steady_state`] for why
    /// that churn is worse than waiting. Always `true` for swu, which has no
    /// interface to be missing.
    fn recreate_interface(&self, runner: &dyn CommandRunner) -> bool;
    /// Reinstate this line's default route if it is missing, returning whether
    /// it had to (`true` = it was missing and has now been repaired).
    ///
    /// Called on every structurally healthy steady-state tick, ahead of the
    /// P-CSCF probe, so a lost route is repaired whatever the probe says
    /// (#120 finding 3).
    ///
    /// "Missing" means missing in `pcscf`'s address family: that is the route
    /// whose absence makes the P-CSCF unreachable, and the other family's
    /// route says nothing about it (#133).
    ///
    /// Separate from [`TunnelEngine::recreate_interface`] because it is the
    /// cheapest possible repair — one `ip route replace`, no disruption —
    /// whereas a rebuild costs a renegotiation and drops any call. A route that
    /// has gone missing is also *the* observed cause of a line that is
    /// structurally perfect and completely unreachable, so it is worth ruling
    /// out before tearing anything down. Always `false` for swu, which manages
    /// its own device and routing through the dialer.
    fn repair_default_route(&self, runner: &dyn CommandRunner, pcscf: &str) -> bool;
    /// Fully restarts this line's primary process from scratch (strongswan:
    /// kill, clear log, remove the stale unqualified pidfile, respawn
    /// charon, `swanctl --load-all`, `swanctl --initiate`; swu: respawn the
    /// dialer). The one place a dead/broken process is actually replaced.
    fn restart_process(&self, runner: &dyn CommandRunner);
    /// The maximum number of establish-time attempts before giving up
    /// (`Some(90)` for swu; `None` for strongswan, which retries forever —
    /// FR-004 of specs/012-strongswan-epdg, "no bounded give-up").
    fn max_establish_attempts(&self) -> Option<u32>;
    /// The attempt-count cadence at which the establish loop re-initiates
    /// mid-wait (`Some(15)` for strongswan; `None` for swu, which has no
    /// periodic action beyond polling — it only ever fully restarts the
    /// dialer, on the steady-state side, never re-initiates in place).
    fn reinitiate_cadence(&self) -> Option<u32>;

    /// Can this line's P-CSCF actually be reached, from inside this line's
    /// namespace, right now?
    ///
    /// The only check here that tests the *data path* rather than its
    /// structure. Everything else asks whether the pieces exist; this asks
    /// whether they carry traffic, which is the question the 2026-08-17 outage
    /// went 8 hours without anyone asking.
    fn pcscf_reachable(&self, runner: &dyn CommandRunner, pcscf: &str) -> bool;
}

/// Outcome of one establish-time tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstablishOutcome {
    StillEstablishing,
    Established {
        pcscf: String,
    },
    /// The process died before establishing — the line is skipped (matches
    /// the original script's `return 1` from `start_line_strongswan`/
    /// `start_line_swu`).
    FatalProcessDied,
    /// swu only: `SWU_MAX_ESTABLISH_ATTEMPTS` reached without connecting.
    FatalTimedOut,
}

/// Advances the establish-time state machine by one tick. 1:1 port of both
/// engines' establish-time `while`/`for` loop bodies (see module docs).
pub fn tick_establishing(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    attempt: &mut u32,
    stuck_without_pcscf: &mut bool,
) -> EstablishOutcome {
    if engine.is_tunnel_established(runner) {
        if let Some(pcscf) = engine.latest_pcscf(runner) {
            return EstablishOutcome::Established { pcscf };
        }
        *stuck_without_pcscf = true;
    } else {
        *stuck_without_pcscf = false;
    }

    if !engine.is_process_alive(runner) {
        return EstablishOutcome::FatalProcessDied;
    }

    *attempt += 1;

    if let Some(max) = engine.max_establish_attempts() {
        if *attempt >= max {
            return EstablishOutcome::FatalTimedOut;
        }
    }

    if let Some(cadence) = engine.reinitiate_cadence() {
        if (*attempt).is_multiple_of(cadence) {
            // Stuck-with-an-established-SA-but-no-P-CSCF gets a full
            // terminate before re-initiating fresh; a plain "still waiting"
            // just re-initiates on top of whatever's there.
            if *stuck_without_pcscf {
                engine.terminate(runner);
            }
            engine.reinitiate(runner);
        }
    }

    EstablishOutcome::StillEstablishing
}

/// Outcome of one steady-state tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteadyOutcome {
    StillUp,
    /// The active connection changed which P-CSCF it reports; the caller
    /// should refresh the P-CSCF source file and restart this line's
    /// vowifi-ims-agent only.
    PcscfChanged {
        new_pcscf: String,
    },
    /// charon is still working on a negotiation, so nothing was done: an
    /// IKE_SA is half-open (specs/100 US3), or the CHILD_SA re-initiate a
    /// first miss sent is still within its time.
    WaitingForNegotiation,
    /// A reachability-inferred recovery was due, and the path to the ePDG is
    /// down, so nothing was issued (specs/100 FR-007).
    Held {
        cause: UplinkHoldCause,
        /// The recovery that was held back.
        skipped: DegradeReason,
        /// Whether the one initiation FR-010a allows an IKE_SA-less hold was
        /// issued by this tick.
        initiated: bool,
        /// Why the IKE_SA was lost; `Some` only on the tick that issued the
        /// held initiation for a lost IKE_SA.
        ike_sa_loss_cause: Option<IkeSaLossCause>,
    },
    /// A reachability-inferred recovery was due and the ePDG is not known to be
    /// dead, so one liveness check was queued and the recovery waits for its
    /// answer (specs/100 FR-004). Not a hold.
    Deferred {
        reason: DegradeReason,
    },
    /// The process/tunnel was unhealthy and a recovery action was already
    /// issued by the time this returns.
    Recovered {
        reason: DegradeReason,
        /// Why the IKE_SA was lost; `Some` only for [`DegradeReason::IkeSaLost`].
        ike_sa_loss_cause: Option<IkeSaLossCause>,
    },
}

/// How long a half-open IKE_SA may show no IKE activity before it is declared
/// stuck (specs/100 R6).
///
/// charon's retransmit gaps are 4, 7.2, 13, 23.3, 42 and 75.6s, so its longest
/// silence while it is still retrying is ~76s. 200s is more than 2.5 times that
/// and more than a 30s tick plus the gap. Age alone says nothing: with
/// `keyingtries = 0` charon keeps one half-open `#N` alive for a whole outage.
pub const HALF_OPEN_STUCK_AFTER: Duration = Duration::from_secs(200);

/// The half-open IKE_SA a line is currently waiting on (specs/100).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HalfOpen {
    /// The `#N` from the `--list-sas` header. A different `#N` replaces the
    /// record.
    pub ike_id: u32,
    /// charon's IKE activity as of the last tick that saw it change.
    pub activity: Option<IkeActivity>,
    /// When the activity last changed, or when this `#N` was first listed.
    /// Idle time is measured from here.
    pub active_at: SystemTime,
    /// Whether the "still negotiating" line was logged for this `#N`.
    pub waiting_logged: bool,
}

/// What the underlay guard currently believes about the path from this host
/// to one line's ePDG (specs/100 data-model.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderlayVerdict {
    /// Nothing suggests the path is down. The default.
    Up,
    /// The host has no usable route or source address to the ePDG.
    NoRoute,
    /// The route is fine, but the ePDG has stopped answering.
    NoResponse,
}

impl UnderlayVerdict {
    /// The hold cause this verdict stands for; `None` for [`Self::Up`].
    pub fn hold_cause(self) -> Option<UplinkHoldCause> {
        match self {
            Self::Up => None,
            Self::NoRoute => Some(UplinkHoldCause::NoRoute),
            Self::NoResponse => Some(UplinkHoldCause::NoResponse),
        }
    }
}

/// An on-demand liveness check in flight (specs/100 FR-004), as the tick sees
/// it: which recovery it is waiting on, whether the ePDG has said anything
/// since the check was sent, and whether its time to answer is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingView {
    pub reason: DegradeReason,
    pub life_since_mark: bool,
    /// [`super::underlay::LIVENESS_WAIT`] has passed since it was sent. Until
    /// then silence is no answer either way.
    pub expired: bool,
}

/// What the loop tells one steady-state tick about the path to the ePDG
/// (specs/100 data-model.md). Built before the tick and never changed by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderlayView {
    /// `[vowifi].underlay_guard`. When false nothing below is consulted.
    pub enabled: bool,
    pub verdict: UnderlayVerdict,
    /// This tick's `ip route get` result, for the one-failure-plus-pending-
    /// recovery rule (data-model row 7).
    pub route_ok: bool,
    pub pending: Option<PendingView>,
    /// The ePDG was heard from within [`RECENT_LIFE`], by this line or another
    /// on the same ePDG (FR-001).
    pub heard_recently: bool,
    /// This tick follows a no-route hold that has just ended, so the path is
    /// back and anything charon was still retrying across it is worth
    /// retrying now rather than after its backoff.
    pub restored: bool,
}

impl UnderlayView {
    /// The guard-off value: always up, so no row of the decision table fires.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            verdict: UnderlayVerdict::Up,
            route_ok: true,
            pending: None,
            heard_recently: true,
            restored: false,
        }
    }

    /// Why a reachability-inferred recovery is held back right now, if it is.
    /// A single failed route check counts too (data-model row 7): a DPD cannot
    /// be sent without a route, so it is not worth one.
    fn holding(&self) -> Option<UplinkHoldCause> {
        if !self.enabled {
            return None;
        }
        match self.verdict.hold_cause() {
            Some(cause) => Some(cause),
            None if !self.route_ok => Some(UplinkHoldCause::NoRoute),
            None => None,
        }
    }
}

/// Whether a recovery is inferred from the ePDG being unreachable, and so is
/// held while the path to it is down (specs/100 FR-007), as opposed to a
/// structural fault in the local plumbing (FR-009). Exhaustive on purpose,
/// with no wildcard arm: a new reason does not compile until it is classified.
pub fn is_reachability_inferred(reason: DegradeReason) -> bool {
    match reason {
        DegradeReason::PcscfUnreachable
        | DegradeReason::PcscfPoolExhausted
        | DegradeReason::IkeSaLost
        | DegradeReason::ChildSaMissing
        | DegradeReason::ChildSaStuck
        | DegradeReason::ChildSaRekeyStuck
        | DegradeReason::IkeSaNegotiationStuck => true,
        DegradeReason::ProcessDied
        | DegradeReason::ViciBroken
        | DegradeReason::TunVanished
        | DegradeReason::TunUnavailable
        | DegradeReason::DefaultRouteMissing
        // A refresh, not a recovery.
        | DegradeReason::PcscfChanged => false,
    }
}

/// How recently the ePDG must have been heard from for a recovery to go ahead
/// without a liveness check first (specs/100 data-model row 8).
pub const RECENT_LIFE: Duration = Duration::from_secs(30);

/// Per-line state the steady-state loop carries between ticks. Owned by the
/// loop (not the engine) so [`tick_steady_state`] stays a pure decision over
/// its inputs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SteadyTracking {
    /// The pool member that answered while the selection did not, on the last
    /// tick (specs/091 R9); `None` when the selection answered or nothing did.
    pub pcscf_via_sibling: Option<std::net::IpAddr>,
    /// Consecutive ticks the P-CSCF was unreachable, and the time they cover.
    pub pcscf_unreachable: Streak,
    /// Consecutive ticks an IKE_SA was listed but had no usable CHILD_SA, and
    /// the time they cover.
    pub child_sa_missing: Streak,
    /// Whether the last tick saw a usable CHILD_SA.
    pub child_sa_usable: bool,
    /// The CHILD_SA (`#N`) a stuck-rekey rebuild was already issued for, so
    /// it gets exactly one (specs/093 FR-006). Like the two fields below, it
    /// only counts while that `#N` is still the line's newest usable CHILD_SA.
    pub rekey_stuck_rebuilt: Option<u32>,
    /// The CHILD_SA whose stuck-rekey deferral for an active call was logged.
    pub rekey_stuck_deferral_logged: Option<u32>,
    /// The CHILD_SA whose missing `expires in` was logged (FR-011).
    pub lifetime_missing_logged: Option<u32>,
    /// When the supervisor last let an agent start on a freshly negotiated
    /// P-CSCF (specs/097 research R3). Only a mark set at or after it counts
    /// towards exhaustion; `None` disables the check. Set by the caller, which
    /// is the one that restarts the agent.
    pub negotiated_at: Option<SystemTime>,
    /// The members a `PcscfPoolExhausted` rebuild was decided on, for the
    /// caller's log line and report. Empty after every other tick.
    pub pool_exhausted: Vec<ExhaustedMember>,
    /// Whether the current exhaustion's deferral for a call has been logged,
    /// so it is said once rather than every tick (specs/097 FR-007).
    pub pool_exhausted_deferral_logged: bool,
    /// Inbound ESP as the last structurally healthy tick saw it, and when
    /// (specs/098 R4). Dropped by every other tick: a snapshot of SAs that a
    /// fault or a respawn has since replaced says nothing about the next loss.
    pub inbound: Option<(Instant, InboundEsp)>,
    /// The verdict on a dead-peer IKE_SA loss, for the caller's log line and
    /// report. `None` after every other tick.
    pub dead_peer: Option<DeadPeerClassification>,
    /// The half-open IKE_SA the line is waiting on, if any (specs/100).
    pub half_open: Option<HalfOpen>,
    /// Whether this hold already issued its one initiation since the IKE_SA
    /// vanished (specs/100 FR-010a). Reset when an IKE_SA is listed again.
    pub held_initiate_sent: bool,
}

impl SteadyTracking {
    /// Forgets every per-CHILD_SA stuck-rekey guard.
    fn clear_rekey_guards(&mut self) {
        self.rekey_stuck_rebuilt = None;
        self.rekey_stuck_deferral_logged = None;
        self.lifetime_missing_logged = None;
    }
}

/// Consecutive missing-CHILD_SA ticks before the IKE_SA is rebuilt: the first
/// only re-initiates the CHILD_SA. No backoff between repeated rebuilds.
pub const CHILD_SA_MISSING_STRIKES: u32 = 2;

/// ...and the time those ticks must cover: two of the strongswan loop's 30s
/// ticks, so the re-initiate the first miss sent has a whole tick to work
/// however fast the loop is polling (5s around a hold, specs/100).
pub const CHILD_SA_MISSING_WINDOW: Duration = Duration::from_secs(60);

/// A run of consecutive failed checks, and the time it covers: each check
/// stands for the pause before it.
///
/// Every strike threshold is stated as both a count and a time. A count alone
/// means a different time at every cadence, and the loops do not share one
/// cadence (swu polls every 5s, strongswan every 30s and every 5s around a
/// hold); a time alone would let a slow loop act on one sample.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Streak {
    pub count: u32,
    pub covered: Duration,
}

impl Streak {
    pub fn hit(&mut self, pause: Duration) {
        self.count = self.count.saturating_add(1);
        self.covered += pause;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// At least `min` checks, covering at least `window`.
    pub fn reached(&self, min: u32, window: Duration) -> bool {
        self.count >= min && self.covered >= window
    }

    /// Whether one more failed check, after `pause`, would reach it.
    fn would_reach(&self, pause: Duration, min: u32, window: Duration) -> bool {
        let mut next = *self;
        next.hit(pause);
        next.reached(min, window)
    }
}

/// Time to hard expiry under which a CHILD_SA that is still the same `#N` has
/// a stuck rekey (specs/093 R2).
///
/// With the daemon's default lifetimes (rekey 3600s with up to 360s of jitter
/// below it, hard expiry 3960s) a natural rekey replaces the CHILD_SA with
/// 360-720s still left, so a CHILD_SA that is still around with less than
/// 300s has not been replaced and will not be. The first 30s tick to see it
/// still has 270s or more, far more than a rebuild takes.
pub const REKEY_STUCK_THRESHOLD: Duration = Duration::from_secs(300);

/// Time to hard expiry under which a stuck-rekey rebuild goes ahead even
/// during a call (specs/093 R6): the call is about to be cut by the expiry
/// anyway, and a planned rebuild recovers sooner than an expiry does. The
/// first tick under it still leaves 60s or more for the rebuild.
pub const REKEY_STUCK_FORCE_MARGIN: Duration = Duration::from_secs(90);

/// IKE_SA rebuilds within the tunnel-failure window that count as flapping.
pub const FLAP_REBUILDS: usize = 3;

/// Sliding window over agent-restarting recoveries (IKE_SA rebuilds), latching
/// "flapping" once [`FLAP_REBUILDS`] land within `window` (specs/087, R7).
///
/// Computed here, where the rebuilds happen, so no timestamps cross the process
/// boundary: the supervisor reports only the latch. Methods take `now` for
/// testability and return whether the latch changed.
#[derive(Debug, Clone)]
pub struct RebuildWindow {
    window: Duration,
    rebuilds: std::collections::VecDeque<std::time::Instant>,
    latched: bool,
}

impl RebuildWindow {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            rebuilds: std::collections::VecDeque::new(),
            latched: false,
        }
    }

    pub fn is_flapping(&self) -> bool {
        self.latched
    }

    fn prune(&mut self, now: std::time::Instant) {
        while self
            .rebuilds
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            self.rebuilds.pop_front();
        }
    }

    /// Records a rebuild at `now`; true if this latched flapping.
    pub fn record(&mut self, now: std::time::Instant) -> bool {
        self.prune(now);
        self.rebuilds.push_back(now);
        if !self.latched && self.rebuilds.len() >= FLAP_REBUILDS {
            self.latched = true;
            return true;
        }
        false
    }

    /// Ages the window; true if it unlatched, which needs a full quiet
    /// `window` since the newest rebuild.
    pub fn tick(&mut self, now: std::time::Instant) -> bool {
        let quiet = self
            .rebuilds
            .back()
            .is_none_or(|t| now.saturating_duration_since(*t) >= self.window);
        self.prune(now);
        if self.latched && quiet {
            self.latched = false;
            self.rebuilds.clear();
            return true;
        }
        false
    }
}

/// One pool member the agent has given up on, as the exhaustion check saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExhaustedMember {
    pub addr: String,
    pub cause: String,
    pub until_unix: u64,
    pub marked_at_unix: u64,
}

/// The one mark cause that never counts: a `503` whose `Retry-After` asked
/// for a pause is the P-CSCF managing its load, not failing (specs/097).
const RETRY_AFTER_CAUSE: &str = "retry-after";

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether the agent has given up on every P-CSCF this line was given since
/// `negotiated_at`, and if so on what grounds (specs/097 FR-003).
///
/// The members are the supervisor's `pool`, or its single selection when it
/// knows no pool. Each must carry a mark in the agent's `state` that has not
/// ended, was set at or after `negotiated_at` (so a mark that outlived the
/// rebuild it caused does not cause another), and is not `retry-after`.
/// Anything missing -- no state file, no baseline, no members, an address that
/// does not parse -- is "not exhausted": an unknown never rebuilds a tunnel.
pub fn exhausted_pool(
    pool: &[String],
    current: &str,
    state: Option<&PoolState>,
    negotiated_at: Option<SystemTime>,
    now: SystemTime,
) -> Option<Vec<ExhaustedMember>> {
    let state = state?;
    let baseline = unix_secs(negotiated_at?);
    let now = unix_secs(now);
    let members: Vec<&str> = match (pool.is_empty(), current.is_empty()) {
        (false, _) => pool.iter().map(String::as_str).collect(),
        (true, false) => vec![current],
        (true, true) => return None,
    };
    members
        .into_iter()
        .map(|member| {
            // Parsed, so `2001:db8::1` and `2001:0db8::1` are one address.
            let addr: std::net::IpAddr = member.parse().ok()?;
            let mark = state.marks.iter().find(|m| m.addr == addr)?;
            let counts = mark.until_unix > now
                && mark.marked_at_unix >= baseline
                && mark.cause != RETRY_AFTER_CAUSE;
            counts.then(|| ExhaustedMember {
                addr: member.to_string(),
                cause: mark.cause.clone(),
                until_unix: mark.until_unix,
                marked_at_unix: mark.marked_at_unix,
            })
        })
        .collect()
}

/// The rebuild both reachability verdicts end in, the probe's
/// (`PcscfUnreachable`) and the agent's (`PcscfPoolExhausted`): recreate the
/// interface, and only if that worked, terminate and re-initiate.
///
/// The same remedy as `TunVanished`, and for the same underlying reason: there
/// is no working data path. A terminate+reinitiate that lands on an interface
/// with no route just churns the carrier and negotiates another SA that
/// carries nothing, which is precisely the state this recovers from.
fn rebuild_tunnel(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    reason: DegradeReason,
) -> SteadyOutcome {
    if !engine.recreate_interface(runner) {
        return SteadyOutcome::Recovered {
            reason: DegradeReason::TunUnavailable,
            ike_sa_loss_cause: None,
        };
    }
    engine.terminate(runner);
    engine.reinitiate(runner);
    SteadyOutcome::Recovered {
        reason,
        ike_sa_loss_cause: None,
    }
}

/// Advances the steady-state state machine by one tick. 1:1 port of both
/// engines' steady-state supervisor loop bodies (see module docs) — the
/// order of checks matches the bash exactly: process-alive first, then
/// engine-specific health, then P-CSCF-change last (a rekey can change the
/// P-CSCF without any of the earlier checks ever firing).
pub fn tick_steady_state(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    current_pcscf: &str,
    tracking: &mut SteadyTracking,
    poll_interval: Duration,
) -> SteadyOutcome {
    tick_steady_state_with_pool(
        engine,
        runner,
        current_pcscf,
        &[],
        tracking,
        poll_interval,
        &|| false,
        &|| None,
    )
}

/// [`tick_steady_state`] for a line whose whole advertised pool is known
/// (specs/091 research R9).
///
/// The reachability check exists to catch a dead data path, and any pool
/// member answering proves the path. Probing only the selection would rebuild
/// the tunnel ~90s after that one P-CSCF died, which is exactly the move the
/// agent's failover makes unnecessary (US1: "without a tunnel rebuild"). The
/// other members are probed only when the selection fails, so a healthy line
/// costs what it always did. With every member dead the rebuild still happens,
/// and it is the spec's "rediscovery": the new negotiation brings a new pool.
///
/// `call_active` and `agent_pool` are what the line's IMS agent says about
/// itself (its call count, its P-CSCF marks), read by the caller from files
/// beside `/tmp/pcscf-N` so this stays a decision over inputs.
#[allow(clippy::too_many_arguments)]
pub fn tick_steady_state_with_pool(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    current_pcscf: &str,
    current_pool: &[String],
    tracking: &mut SteadyTracking,
    poll_interval: Duration,
    call_active: &dyn Fn() -> bool,
    agent_pool: &dyn Fn() -> Option<PoolState>,
) -> SteadyOutcome {
    tick_steady_state_underlay(
        engine,
        runner,
        current_pcscf,
        current_pool,
        tracking,
        poll_interval,
        call_active,
        agent_pool,
        &UnderlayView::disabled(),
    )
}

/// [`tick_steady_state_with_pool`] for a line whose path to the ePDG the loop
/// is watching (specs/100).
///
/// `underlay` says whether that path is down. When it is, a recovery that is
/// only *inferred* from the ePDG being unreachable (FR-007) is returned as
/// [`SteadyOutcome::Held`] instead of being issued, and when the ePDG is not
/// known to be dead one liveness check is queued first and the recovery
/// returned as [`SteadyOutcome::Deferred`] (FR-004). Structural recoveries
/// (FR-009) never consult it. With [`UnderlayView::disabled`] every branch
/// below behaves exactly as it did before the guard existed.
#[allow(clippy::too_many_arguments)]
pub fn tick_steady_state_underlay(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    current_pcscf: &str,
    current_pool: &[String],
    tracking: &mut SteadyTracking,
    poll_interval: Duration,
    call_active: &dyn Fn() -> bool,
    agent_pool: &dyn Fn() -> Option<PoolState>,
    underlay: &UnderlayView,
) -> SteadyOutcome {
    tracking.pool_exhausted.clear();
    tracking.dead_peer = None;
    if !engine.is_process_alive(runner) {
        tracking.inbound = None;
        // A respawned charon starts a fresh connection that gets its own first
        // retry, so a miss counted against the old one must not carry over.
        tracking.child_sa_missing.reset();
        tracking.child_sa_usable = false;
        tracking.clear_rekey_guards();
        engine.restart_process(runner);
        return SteadyOutcome::Recovered {
            reason: DegradeReason::ProcessDied,
            ike_sa_loss_cause: None,
        };
    }

    let health = engine.steady_state_health(runner);
    // Taken from the healthy listing only, and before anything else in this
    // tick acts on it; consumed below if this tick finds the IKE_SA gone.
    let last_inbound = tracking.inbound.take();
    if matches!(health, SteadyStateHealth::Ok(_)) {
        tracking.inbound = engine.inbound_esp().map(|esp| (Instant::now(), esp));
    }
    // Only a fully healthy structural check counts as a usable CHILD_SA; an
    // unreachable P-CSCF later in the tick does not undo that (specs/086).
    tracking.child_sa_usable = matches!(health, SteadyStateHealth::Ok(_));
    if health != SteadyStateHealth::ChildSaMissing {
        tracking.child_sa_missing.reset();
    }
    if !matches!(health, SteadyStateHealth::IkeSaConnecting { .. }) {
        tracking.half_open = None;
    }
    if health != SteadyStateHealth::IkeSaMissing {
        tracking.held_initiate_sent = false;
    }
    if !matches!(health, SteadyStateHealth::Ok(Some(_))) {
        tracking.clear_rekey_guards();
    }
    match health {
        SteadyStateHealth::ViciBroken => {
            engine.restart_process(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::ViciBroken,
                ike_sa_loss_cause: None,
            };
        }
        SteadyStateHealth::TunVanished => {
            // 1:1 port of the original script's own recovery: recreate the
            // XFRM interface (idempotent — matches `ensure_epdg_interface`)
            // BEFORE terminate+reinitiate. Missing this call was a real bug
            // found live: without it, every subsequent negotiation kept
            // succeeding at the IKE/CHILD_SA level while silently failing to
            // install a working data path onto an interface that was never
            // actually recreated — a fresh IKE_SA every steady-state tick,
            // forever, never actually fixing the underlying problem.
            //
            // The terminate+reinitiate is conditional on that recreation
            // having actually worked. Measured live 2026-07-31: when the
            // container is replaced, the previous run's `ims<N>` namespaces
            // — and the `tun23-<N>` devices inside them, which register their
            // `if_id` in the namespace they were *created* in, the host's —
            // survive about two and a half minutes. Nothing shortens it: the
            // shutdown plan's `ip netns del` runs, the container exits 0, and
            // a stopped deployment still held both ids 2m29s later. Until the
            // kernel reaps them the id is refused and no interface can exist,
            // so terminating each tick only churned the carrier without ever
            // getting closer to a data path: eight and six IKE_SA setups
            // across two startups on the two lines, against two and two once
            // this branch stopped doing it. Waiting is free by comparison: a
            // line with no tunnel interface carries no traffic either way,
            // and the first tick after the id frees recreates it and recovers
            // normally — an 11s startup once the old namespaces are gone,
            // against 163s and 195s when they are not.
            if !engine.recreate_interface(runner) {
                return SteadyOutcome::Recovered {
                    reason: DegradeReason::TunUnavailable,
                    ike_sa_loss_cause: None,
                };
            }
            engine.terminate(runner);
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::TunVanished,
                ike_sa_loss_cause: None,
            };
        }
        SteadyStateHealth::IkeSaMissing => {
            // No IKE_SA to terminate: re-initiate builds a fresh one. The
            // strike count is reset above, so a later CHILD_SA loss starts
            // from its first strike.
            // Read before re-initiating: the parser resets on the next
            // establishment, so a slow tick would otherwise lose the cause.
            let cause = engine.ike_sa_loss_cause(runner);
            // While the path to the ePDG is down nothing the carrier sends
            // can reach us, so a re-initiate would only feed the churn. One is
            // allowed per hold, though: without an IKE_SA nothing would be sent
            // to the ePDG, and a "no response" hold could never see the answer
            // that ends it (specs/100 FR-010a). charon's own retries carry it
            // from there.
            if let Some(held) = underlay.holding() {
                let initiated = !tracking.held_initiate_sent;
                if initiated {
                    tracking.held_initiate_sent = true;
                    engine.reinitiate(runner);
                }
                // The peer's silence is explained by the path, not by the peer
                // (FR-022).
                if initiated && cause == IkeSaLossCause::DeadPeer {
                    tracking.dead_peer = Some(DeadPeerClassification {
                        verdict: DeadPeerVerdict::Uplink,
                        snapshot_age: None,
                        last_in_before_loss: None,
                        packets: None,
                        rx_since_snapshot: None,
                    });
                }
                return SteadyOutcome::Held {
                    cause: held,
                    skipped: DegradeReason::IkeSaLost,
                    initiated,
                    ike_sa_loss_cause: initiated.then_some(cause),
                };
            }
            if cause == IkeSaLossCause::DeadPeer {
                tracking.dead_peer = Some(classify_dead_peer(
                    last_inbound,
                    Instant::now(),
                    engine.tun_rx_packets(),
                ));
            }
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaLost,
                ike_sa_loss_cause: Some(cause),
            };
        }
        SteadyStateHealth::IkeSaConnecting { ike_id } => {
            return tick_half_open(engine, runner, ike_id, tracking, underlay);
        }
        SteadyStateHealth::ChildSaMissing => {
            let stuck = tracking.child_sa_missing.would_reach(
                poll_interval,
                CHILD_SA_MISSING_STRIKES,
                CHILD_SA_MISSING_WINDOW,
            );
            if !stuck && tracking.child_sa_missing.count > 0 {
                // The first miss re-initiated it, and that has not had its
                // time yet: one CHILD_SA request per miss would be one every
                // 5s around a hold.
                tracking.child_sa_missing.hit(poll_interval);
                return SteadyOutcome::WaitingForNegotiation;
            }
            let reason = if stuck {
                DegradeReason::ChildSaStuck
            } else {
                DegradeReason::ChildSaMissing
            };
            if let Some(outcome) = underlay_gate(engine, runner, underlay, reason) {
                // A strike earned against a dead path means nothing once it is
                // back: the next one has to be earned afresh.
                if matches!(outcome, SteadyOutcome::Held { .. }) {
                    tracking.child_sa_missing.reset();
                }
                return outcome;
            }
            tracking.child_sa_missing.hit(poll_interval);
            if tracking
                .child_sa_missing
                .reached(CHILD_SA_MISSING_STRIKES, CHILD_SA_MISSING_WINDOW)
            {
                // Re-initiating inside the surviving IKE_SA did not bring the
                // CHILD_SA back; only a fresh IKE_SA will.
                tracking.child_sa_missing.reset();
                engine.terminate(runner);
                engine.reinitiate(runner);
                return SteadyOutcome::Recovered {
                    reason: DegradeReason::ChildSaStuck,
                    ike_sa_loss_cause: None,
                };
            }
            // First miss: re-initiate only, without restarting the agent.
            engine.reinitiate(runner);
            return SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            };
        }
        SteadyStateHealth::Ok(None) => {}
        SteadyStateHealth::Ok(Some(child)) => {
            if let Some(outcome) =
                check_rekey_stuck(engine, runner, child, tracking, call_active, underlay)
            {
                // A held rebuild must not hold back the route repair below
                // too: that one is structural and never held (FR-009), and
                // the hold can last 15 minutes.
                if matches!(outcome, SteadyOutcome::Held { .. }) {
                    if let Some(repaired) =
                        repair_default_route(engine, runner, current_pcscf, tracking)
                    {
                        return repaired;
                    }
                }
                return outcome;
            }
        }
    }

    // Reachability is checked last, and deliberately *after* the P-CSCF-changed
    // branch above: when the carrier moves the P-CSCF, the old address becoming
    // unreachable is expected, and refreshing to the new one is the right
    // remedy rather than rebuilding the tunnel.
    if let Some(latest) = engine.latest_pcscf(runner) {
        if latest != current_pcscf {
            return SteadyOutcome::PcscfChanged { new_pcscf: latest };
        }
    }

    // Is the route simply gone? That is a millisecond repair with no
    // renegotiation, against a rebuild that renegotiates the SA -- and it is
    // the one fault that presents with every structural check passing.
    //
    // It is also what a rebuild cannot fix. The kernel deletes an interface's
    // IPv4 default route when its last IPv4 address goes, so the `terminate`
    // in the unreachable branch below destroys the route `recreate_interface`
    // installs moments earlier. On 2026-08-19 that loop ran 202 times over six
    // hours without ever restoring the data path.
    //
    // Checked on every tick, ahead of the probe, because it is structural: it
    // used to run only once the probe had failed, so without the probe (#120)
    // a lost route was never put back -- the probes-off soak left a line down
    // for 10 minutes (#120 finding 3).
    //
    // The current selection's family, because that is the address the line
    // registers through; a sibling only matters once it has been chosen. No
    // selection yet means no family to check, as for the probe below.
    if let Some(repaired) = repair_default_route(engine, runner, current_pcscf, tracking) {
        return repaired;
    }

    // Has the line's IMS agent given up on every P-CSCF it was given? It is
    // the one that actually signals through them, over the protected Gm
    // ports, so when it says they are all dead it wins over the probe below,
    // which only knocks on the unprotected one (specs/097 FR-005). And it
    // answers the case the probe cannot help with once it is gone: a carrier
    // with a single P-CSCF, where the agent has no sibling to fail over to and
    // only a new negotiation brings a new address (#120 finding 4).
    //
    // One tick is enough. A mark is already a debounced verdict -- three dead
    // keepalives, or a REGISTER that timed out -- and waiting longer only
    // lengthens an outage on a line that is already deaf.
    let exhausted = exhausted_pool(
        current_pool,
        current_pcscf,
        agent_pool().as_ref(),
        tracking.negotiated_at,
        SystemTime::now(),
    );
    match exhausted {
        Some(members) if !call_active() => {
            if let Some(outcome) =
                underlay_gate(engine, runner, underlay, DegradeReason::PcscfPoolExhausted)
            {
                return outcome;
            }
            tracking.pool_exhausted_deferral_logged = false;
            tracking.pcscf_unreachable.reset();
            let outcome = rebuild_tunnel(engine, runner, DegradeReason::PcscfPoolExhausted);
            if matches!(
                outcome,
                SteadyOutcome::Recovered {
                    reason: DegradeReason::PcscfPoolExhausted,
                    ..
                }
            ) {
                tracking.pool_exhausted = members;
            }
            return outcome;
        }
        // Held for a call, however long it lasts (clarification Q1): media can
        // still be flowing with signalling dead, and the agent holds back its
        // own re-registration during a call for the same reason.
        Some(_) => {
            if !tracking.pool_exhausted_deferral_logged {
                tracking.pool_exhausted_deferral_logged = true;
                println!(
                    "{} every P-CSCF is marked unavailable; deferring the tunnel rebuild while a call is active",
                    engine.log_prefix()
                );
            }
        }
        None => tracking.pool_exhausted_deferral_logged = false,
    }

    let selection_reachable = engine.pcscf_reachable(runner, current_pcscf);
    let sibling = if selection_reachable {
        None
    } else {
        current_pool
            .iter()
            .filter(|a| a.as_str() != current_pcscf)
            .find(|a| engine.pcscf_reachable(runner, a))
            .and_then(|a| a.parse().ok())
    };
    tracking.pcscf_via_sibling = sibling;
    if selection_reachable || tracking.pcscf_via_sibling.is_some() {
        tracking.pcscf_unreachable.reset();
    } else {
        // The route was checked above, this same tick: unreachable with it in
        // place is some other fault.
        tracking.pcscf_unreachable.hit(poll_interval);
        // Measured in real time, so both engines, and one loop whose cadence
        // changes, wait the same ~90s rather than the same number of ticks.
        // At a fixed cadence this is `pcscf_unreachable_strikes` ticks.
        if tracking
            .pcscf_unreachable
            .reached(PCSCF_UNREACHABLE_MIN_STRIKES, PCSCF_UNREACHABLE_WINDOW)
        {
            if let Some(outcome) =
                underlay_gate(engine, runner, underlay, DegradeReason::PcscfUnreachable)
            {
                // Strikes counted against a dead path are not evidence once it
                // is back.
                if matches!(outcome, SteadyOutcome::Held { .. }) {
                    tracking.pcscf_unreachable.reset();
                }
                return outcome;
            }
            tracking.pcscf_unreachable.reset();
            return rebuild_tunnel(engine, runner, DegradeReason::PcscfUnreachable);
        }
    }

    SteadyOutcome::StillUp
}

/// Puts the namespace default route back if it is gone, for the current
/// selection's family: `Some` when it was repaired.
fn repair_default_route(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    current_pcscf: &str,
    tracking: &mut SteadyTracking,
) -> Option<SteadyOutcome> {
    if current_pcscf.is_empty() || !engine.repair_default_route(runner, current_pcscf) {
        return None;
    }
    tracking.pcscf_unreachable.reset();
    Some(SteadyOutcome::Recovered {
        reason: DegradeReason::DefaultRouteMissing,
        ike_sa_loss_cause: None,
    })
}

/// What the guard says about a reachability-inferred recovery the tick is
/// about to take (specs/100 data-model rows 5-8): `Some` to return instead of
/// issuing it, `None` to go ahead.
///
/// Never called for `IkeSaLost`, whose held branch also owns FR-010a's single
/// initiation, nor for a structural reason.
fn underlay_gate(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    underlay: &UnderlayView,
    reason: DegradeReason,
) -> Option<SteadyOutcome> {
    debug_assert!(is_reachability_inferred(reason) && reason != DegradeReason::IkeSaLost);
    if !underlay.enabled {
        return None;
    }
    // Rows 5 and 7: the path is down, or the route check failed once on a tick
    // that is about to act on it.
    if let Some(cause) = underlay.holding() {
        return Some(SteadyOutcome::Held {
            cause,
            skipped: reason,
            initiated: false,
            ike_sa_loss_cause: None,
        });
    }
    // Row 6: a liveness check is already out. An ePDG that has said anything
    // since it was sent is alive, so the recovery is a genuine one; one that
    // has stayed silent for the whole wait is not answering. Before that,
    // silence settles nothing: keep waiting, without a second check.
    if let Some(pending) = underlay.pending {
        return if pending.life_since_mark {
            None
        } else if !pending.expired {
            Some(SteadyOutcome::Deferred { reason })
        } else {
            Some(SteadyOutcome::Held {
                cause: UplinkHoldCause::NoResponse,
                skipped: reason,
                initiated: false,
                ike_sa_loss_cause: None,
            })
        };
    }
    // Row 8: nothing proves the ePDG alive, and there is an IKE_SA to ask.
    let esp_recent = engine
        .inbound_esp()
        .and_then(|esp| esp.last_in_age)
        .is_some_and(|age| age < RECENT_LIFE);
    let established = matches!(engine.ike_header(), Some((_, IkeHeaderState::Established)));
    if established && !underlay.heard_recently && !esp_recent {
        let sent = engine.send_dpd(runner);
        println!(
            "{} the ePDG has been silent for {}s; {} before {reason:?}",
            engine.log_prefix(),
            RECENT_LIFE.as_secs(),
            if sent {
                "checking it is alive (DPD sent)"
            } else {
                "could not send a DPD; checking from the log alone"
            }
        );
        return Some(SteadyOutcome::Deferred { reason });
    }
    None
}

/// The decision for a half-open IKE_SA (specs/100 US3, data-model rows 3-4):
/// leave it alone while charon has logged IKE activity for the connection
/// within [`HALF_OPEN_STUCK_AFTER`], terminate and re-initiate it once it has
/// not.
///
/// The old behaviour counted it as a missing CHILD_SA and terminated it on the
/// second tick, which in an outage killed the very attempt charon was still
/// retrying (#141: every ~80s).
fn tick_half_open(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    ike_id: u32,
    tracking: &mut SteadyTracking,
    underlay: &UnderlayView,
) -> SteadyOutcome {
    let now = SystemTime::now();
    let activity = engine.ike_activity(runner);
    let mut half_open = match tracking.half_open.take() {
        Some(h) if h.ike_id == ike_id => h,
        _ => HalfOpen {
            ike_id,
            activity: activity.clone(),
            active_at: now,
            waiting_logged: false,
        },
    };
    if activity != half_open.activity {
        half_open.activity = activity;
        half_open.active_at = now;
    }
    // The no-route hold that kept this IKE_SA's retransmits from reaching the
    // ePDG has just ended. charon backs off up to ~76s between retransmits, so
    // waiting for its next one can leave the line down for a minute with the
    // path already back: send a fresh IKE_SA_INIT now instead (specs/100
    // SC-003). A hold that ended because the ePDG was heard from does not get
    // here: an IKE_SA that answered is progressing, not waiting.
    if underlay.enabled && underlay.restored {
        println!(
            "{} IKE_SA #{ike_id} was half-open across a no-route hold that has just ended; re-initiating it now rather than waiting out charon's backoff",
            engine.log_prefix()
        );
        tracking.half_open = None;
        engine.terminate(runner);
        engine.reinitiate(runner);
        return SteadyOutcome::Recovered {
            reason: DegradeReason::IkeSaNegotiationStuck,
            ike_sa_loss_cause: None,
        };
    }
    let idle = now.duration_since(half_open.active_at).unwrap_or_default();
    if idle > HALF_OPEN_STUCK_AFTER {
        if let Some(outcome) = underlay_gate(
            engine,
            runner,
            underlay,
            DegradeReason::IkeSaNegotiationStuck,
        ) {
            // The record stays: it is still the same half-open IKE_SA.
            tracking.half_open = Some(half_open);
            return outcome;
        }
        println!(
            "{} IKE_SA #{ike_id} half-open with no IKE activity for {}s; terminating and re-initiating",
            engine.log_prefix(),
            idle.as_secs()
        );
        tracking.half_open = None;
        engine.terminate(runner);
        engine.reinitiate(runner);
        return SteadyOutcome::Recovered {
            reason: DegradeReason::IkeSaNegotiationStuck,
            ike_sa_loss_cause: None,
        };
    }
    if !half_open.waiting_logged {
        half_open.waiting_logged = true;
        println!(
            "{} IKE_SA #{ike_id} still negotiating; leaving it to charon",
            engine.log_prefix()
        );
    }
    tracking.half_open = Some(half_open);
    SteadyOutcome::WaitingForNegotiation
}

/// The stuck-rekey decision for a structurally healthy tick (specs/093,
/// data-model.md's decision table): `Some` when it rebuilt the tunnel.
///
/// The trigger is the newest CHILD_SA's own time to hard expiry, never its
/// state. A refused rekey is answered within a round trip and the CHILD_SA
/// reads `INSTALLED` again until the next retry a few seconds later, so a 30s
/// tick would almost never catch it `REKEYING` (#107: 105 refusals in 11
/// minutes). Being the same `#N` with less than [`REKEY_STUCK_THRESHOLD`] left
/// is what every failed rekey has in common, whatever the carrier answered.
fn check_rekey_stuck(
    engine: &dyn TunnelEngine,
    runner: &dyn CommandRunner,
    child: ChildSaLifetime,
    tracking: &mut SteadyTracking,
    call_active: &dyn Fn() -> bool,
    underlay: &UnderlayView,
) -> Option<SteadyOutcome> {
    let id = child.unique_id;
    // A guard set for an earlier CHILD_SA means nothing for this one.
    for guard in [
        &mut tracking.rekey_stuck_rebuilt,
        &mut tracking.rekey_stuck_deferral_logged,
        &mut tracking.lifetime_missing_logged,
    ] {
        if guard.is_some_and(|g| g != id) {
            *guard = None;
        }
    }

    let Some(expires_in) = child.expires_in else {
        if tracking.lifetime_missing_logged != Some(id) {
            tracking.lifetime_missing_logged = Some(id);
            println!(
                "{} CHILD_SA #{id} reports no expiry; stuck-rekey detection is off for it",
                engine.log_prefix()
            );
        }
        return None;
    };
    if expires_in > REKEY_STUCK_THRESHOLD.as_secs() as i64
        || tracking.rekey_stuck_rebuilt == Some(id)
    {
        return None;
    }

    // Read only now, on a tick that would rebuild: a healthy line never
    // touches the call-state file (specs/093 contracts/files.md).
    let in_call = call_active();
    let forced = expires_in <= REKEY_STUCK_FORCE_MARGIN.as_secs() as i64;
    if in_call && !forced {
        if tracking.rekey_stuck_deferral_logged != Some(id) {
            tracking.rekey_stuck_deferral_logged = Some(id);
            println!(
                "{} CHILD_SA #{id} rekey overdue (expires in {expires_in}s); deferring the rebuild while a call is active (forced at {}s)",
                engine.log_prefix(),
                REKEY_STUCK_FORCE_MARGIN.as_secs()
            );
        }
        return None;
    }

    if let Some(outcome) = underlay_gate(engine, runner, underlay, DegradeReason::ChildSaRekeyStuck)
    {
        return Some(outcome);
    }
    let rejection = engine
        .child_sa_rejection(runner)
        .map(|n| format!(", last rejection N({n})"))
        .unwrap_or_default();
    let despite = if in_call {
        " despite an active call"
    } else {
        ""
    };
    println!(
        "{} CHILD_SA #{id} rekey overdue (expires in {expires_in}s{rejection}); rebuilding before the hard expiry{despite}",
        engine.log_prefix()
    );
    tracking.rekey_stuck_rebuilt = Some(id);
    engine.terminate(runner);
    engine.reinitiate(runner);
    Some(SteadyOutcome::Recovered {
        reason: DegradeReason::ChildSaRekeyStuck,
        ike_sa_loss_cause: None,
    })
}

/// Whether a [`SteadyOutcome::Recovered`] reason also means "restart this
/// line's vowifi-ims-agent" — matches the original script exactly: every
/// recovery path does (`IkeSaLost` included: the new IKE_SA means a new VIP
/// and P-CSCF pool), EXCEPT a bare `ChildSaMissing` re-initiate (a CHILD_SA
/// lost under a live, listed IKE_SA).
///
/// `TunUnavailable` is the one addition to that rule. The agent's whole job is
/// to route over a tunnel interface which, in that state, does not exist and
/// cannot be made to exist yet, so killing it only guarantees it fails the same
/// way on restart.
///
/// Worth knowing what this does *not* buy: measured live 2026-07-31, the agent
/// restart count over a container replacement did not move (48 across two
/// startups, before and after). Nearly all of those are the agent's own
/// crash-loop — it starts, cannot reach its P-CSCF without an interface, exits,
/// and its supervisor restarts it 5s later — which this does not touch. The
/// tick-driven kills removed here are a handful on top of that. The change is
/// kept because killing a process over a condition it cannot affect is wrong on
/// its own terms, not because it measurably improved anything.
pub fn recovery_restarts_agent(reason: DegradeReason) -> bool {
    !matches!(
        reason,
        DegradeReason::ChildSaMissing | DegradeReason::TunUnavailable
    )
}

/// Whether a recovery starts a new IKE negotiation, i.e. whether a fresh P-CSCF
/// batch will follow and the recovery counts as an IKE_SA rebuild.
///
/// Narrower than [`recovery_restarts_agent`]: `DefaultRouteMissing` restarts
/// the agent but only repairs a route, so waiting for a new negotiation would
/// burn the whole wait for nothing, and three route repairs are not a flapping
/// tunnel.
pub fn recovery_renegotiates(reason: DegradeReason) -> bool {
    recovery_restarts_agent(reason) && reason != DegradeReason::DefaultRouteMissing
}

#[cfg(test)]
mod tests {
    mod dead_peer {
        use super::super::*;

        const RX_AT_SNAPSHOT: u64 = 1000;

        fn esp(age: Option<u64>) -> InboundEsp {
            InboundEsp {
                last_in_age: age.map(Duration::from_secs),
                packets: 42,
                tun_rx_packets: Some(RX_AT_SNAPSHOT),
            }
        }

        /// Classify with the snapshot taken `snapshot_age` before the loss
        /// was seen, its last inbound packet `age` before that, and nothing
        /// received on the tunnel interface in between.
        fn verdict(snapshot_age: u64, age: Option<u64>) -> DeadPeerVerdict {
            verdict_rx(snapshot_age, age, Some(RX_AT_SNAPSHOT))
        }

        /// As [`verdict`], with the loss tick's counter reading `rx_now`.
        fn verdict_rx(snapshot_age: u64, age: Option<u64>, rx_now: Option<u64>) -> DeadPeerVerdict {
            let now = Instant::now() + Duration::from_secs(10_000);
            let taken = now - Duration::from_secs(snapshot_age);
            classify_dead_peer(Some((taken, esp(age))), now, rx_now).verdict
        }

        #[test]
        fn no_snapshot_is_unknown_never_false() {
            let c = classify_dead_peer(None, Instant::now(), Some(5));
            assert_eq!(c.verdict, DeadPeerVerdict::Unknown);
            assert_eq!(c.snapshot_age, None);
            assert_eq!(c.packets, None);
        }

        #[test]
        fn esp_while_charon_retransmitted_is_false() {
            // Run C: OPTIONS answered every 120s inside the 165s window.
            assert_eq!(verdict(12, Some(30)), DeadPeerVerdict::False);
            assert_eq!(verdict(30, Some(134)), DeadPeerVerdict::False);
        }

        #[test]
        fn silence_from_before_the_request_could_go_out_is_real() {
            // A dead ePDG at the default: dpd_delay (120s) of silence, then
            // 165s of retransmits, seen within one 30s tick.
            assert_eq!(verdict(20, Some(270)), DeadPeerVerdict::Real);
            assert_eq!(verdict(0, Some(165)), DeadPeerVerdict::Real);
        }

        #[test]
        fn the_band_between_is_unknown() {
            // L ≤ T − G but L > S − G: either side of the first request.
            assert_eq!(verdict(30, Some(140)), DeadPeerVerdict::Unknown);
            assert_eq!(verdict(30, Some(164)), DeadPeerVerdict::Unknown);
        }

        #[test]
        fn boundaries_follow_the_contract() {
            // T − L == G exactly is not "after T − G".
            assert_eq!(verdict(15, Some(150)), DeadPeerVerdict::Unknown);
            assert_eq!(verdict(15, Some(149)), DeadPeerVerdict::False);
            // S − L == G exactly is "at or before S − G".
            assert_eq!(verdict(15, Some(165)), DeadPeerVerdict::Real);
        }

        #[test]
        fn no_inbound_use_on_any_listed_sa_is_unknown() {
            assert_eq!(verdict(10, None), DeadPeerVerdict::Unknown);
        }

        #[test]
        fn the_evidence_is_carried_for_the_log() {
            let now = Instant::now() + Duration::from_secs(100);
            let c = classify_dead_peer(
                Some((now - Duration::from_secs(12), esp(Some(30)))),
                now,
                Some(RX_AT_SNAPSHOT + 3),
            );
            assert_eq!(c.snapshot_age, Some(Duration::from_secs(12)));
            assert_eq!(c.last_in_before_loss, Some(Duration::from_secs(42)));
            assert_eq!(c.packets, Some(42));
            assert_eq!(c.rx_since_snapshot, Some(3));
        }

        #[test]
        fn esp_resuming_after_the_snapshot_is_false_not_real() {
            // 270s of silence at the snapshot, ESP back 10s later, charon
            // giving up 20s after that: the path recovered inside the window.
            assert_eq!(
                verdict_rx(30, Some(270), Some(RX_AT_SNAPSHOT + 4)),
                DeadPeerVerdict::False
            );
            // It also settles the band the snapshot alone cannot.
            assert_eq!(
                verdict_rx(30, Some(140), Some(RX_AT_SNAPSHOT + 1)),
                DeadPeerVerdict::False
            );
            assert_eq!(
                verdict_rx(10, None, Some(RX_AT_SNAPSHOT + 1)),
                DeadPeerVerdict::False
            );
        }

        #[test]
        fn real_needs_the_counter_to_cover_the_rest_of_the_window() {
            assert_eq!(verdict_rx(20, Some(270), None), DeadPeerVerdict::Unknown);
            let now = Instant::now() + Duration::from_secs(10_000);
            let unread_then = InboundEsp {
                tun_rx_packets: None,
                ..esp(Some(270))
            };
            assert_eq!(
                classify_dead_peer(
                    Some((now - Duration::from_secs(20), unread_then)),
                    now,
                    Some(9)
                )
                .verdict,
                DeadPeerVerdict::Unknown
            );
            // A counter that went back (the interface was recreated) proves
            // nothing.
            assert_eq!(
                verdict_rx(20, Some(270), Some(RX_AT_SNAPSHOT - 1)),
                DeadPeerVerdict::Unknown
            );
        }

        #[test]
        fn packets_after_a_snapshot_older_than_the_window_prove_nothing() {
            // `S ≤ T − G`: a packet just after `S` may predate the request.
            assert_eq!(
                verdict_rx(165, Some(10), Some(RX_AT_SNAPSHOT + 1)),
                DeadPeerVerdict::Unknown
            );
        }
    }

    use super::*;
    use crate::supervise::runner::MockCommandRunner;
    use std::cell::RefCell;

    /// `tick_steady_state` at the strongswan cadence, which is what almost every
    /// test here means. Shadows the real one so the cases that do not care about
    /// the poll interval stay readable; the ones that *do* care call
    /// `super::tick_steady_state` explicitly with the interval under test.
    fn tick_steady_state(
        engine: &dyn TunnelEngine,
        runner: &dyn CommandRunner,
        current_pcscf: &str,
        tracking: &mut SteadyTracking,
    ) -> SteadyOutcome {
        super::tick_steady_state(
            engine,
            runner,
            current_pcscf,
            tracking,
            STEADY_STATE_POLL_INTERVAL,
        )
    }

    // MOCK JUSTIFICATION (constitution Principle I): TunnelEngine's real
    // implementations (StrongswanEngine/SwuEngine) drive real charon/pcscd/
    // swanctl/a live modem — none available in CI. This fake engine lets the
    // *state machine* (tick_establishing/tick_steady_state) be tested against
    // every transition in data-model.md's table directly, independent of
    // engine wiring.
    struct FakeEngine {
        /// Whether the data path works. Separate from every structural field
        /// below, because the whole point is that structure can be perfect
        /// while nothing is reachable.
        pcscf_reachable: RefCell<bool>,
        /// When set, exactly these addresses answer, overriding
        /// `pcscf_reachable` (specs/091 R9).
        reachable_addrs: RefCell<Option<Vec<String>>>,
        /// Every address probed, in order.
        probed: RefCell<Vec<String>>,
        /// Whether this line's default route has gone missing. Separate from
        /// `pcscf_reachable` because the whole point is that a missing route is
        /// one specific *cause* of unreachability with its own cheap remedy.
        route_missing: RefCell<bool>,
        route_repairs: RefCell<u32>,
        /// Every `repair_default_route` call, including the ones that found
        /// nothing missing -- `route_repairs` counts only actual repairs.
        repair_calls: RefCell<u32>,
        established: RefCell<bool>,
        pcscf: RefCell<Option<String>>,
        pool: RefCell<Vec<String>>,
        alive: RefCell<bool>,
        health: RefCell<SteadyStateHealth>,
        max_attempts: Option<u32>,
        reinitiate_cadence: Option<u32>,
        terminate_calls: RefCell<u32>,
        reinitiate_calls: RefCell<u32>,
        restart_calls: RefCell<u32>,
        recreate_interface_calls: RefCell<u32>,
        /// Whether `recreate_interface` reports the interface as present
        /// afterwards — `false` models a line whose `if_id` is still held by
        /// a not-yet-reaped previous run.
        recreate_ok: bool,
        /// Recovery calls in the order they were issued. Counts alone cannot
        /// express the ordering the TunVanished branch depends on.
        call_order: RefCell<Vec<&'static str>>,
        /// What `child_sa_rejection` reports (specs/093 R4).
        rejection: RefCell<Option<String>>,
        /// What `inbound_esp` reports (specs/098).
        inbound: RefCell<Option<InboundEsp>>,
        /// What `tun_rx_packets` reports (specs/098).
        tun_rx: RefCell<Option<u64>>,
        /// What `ike_sa_loss_cause` reports.
        loss_cause: RefCell<IkeSaLossCause>,
        /// What `ike_header` reports (specs/100).
        ike_header: RefCell<Option<(u32, IkeHeaderState)>>,
        /// What `ike_activity` reports (specs/100).
        activity: RefCell<Option<IkeActivity>>,
        /// What `route_ok` reports (specs/100).
        route_ok: RefCell<bool>,
        /// What `ike_evidence_since` reports (specs/100).
        evidence: RefCell<IkeEvidence>,
        /// Every `send_dpd` call.
        dpd_calls: RefCell<u32>,
        /// Whether `send_dpd` reports the check as queued.
        dpd_accepted: RefCell<bool>,
    }

    impl Default for FakeEngine {
        fn default() -> Self {
            Self {
                established: RefCell::new(false),
                pcscf: RefCell::new(None),
                pool: RefCell::new(Vec::new()),
                // Default reachable: every pre-existing test asserts on the
                // structural checks and must keep passing unchanged.
                pcscf_reachable: RefCell::new(true),
                reachable_addrs: RefCell::new(None),
                probed: RefCell::new(Vec::new()),
                // Default present: an unreachable P-CSCF in a pre-existing test
                // means "unreachable for some other reason", so the route repair
                // must not silently absorb those cases.
                route_missing: RefCell::new(false),
                route_repairs: RefCell::new(0),
                repair_calls: RefCell::new(0),
                alive: RefCell::new(true),
                health: RefCell::new(SteadyStateHealth::Ok(None)),
                max_attempts: None,
                reinitiate_cadence: Some(STRONGSWAN_REINITIATE_EVERY),
                terminate_calls: RefCell::new(0),
                reinitiate_calls: RefCell::new(0),
                restart_calls: RefCell::new(0),
                recreate_interface_calls: RefCell::new(0),
                recreate_ok: true,
                call_order: RefCell::new(Vec::new()),
                rejection: RefCell::new(None),
                inbound: RefCell::new(None),
                tun_rx: RefCell::new(None),
                loss_cause: RefCell::new(IkeSaLossCause::Unknown),
                ike_header: RefCell::new(None),
                activity: RefCell::new(None),
                route_ok: RefCell::new(true),
                evidence: RefCell::new(IkeEvidence::LIFE),
                dpd_calls: RefCell::new(0),
                dpd_accepted: RefCell::new(true),
            }
        }
    }

    impl TunnelEngine for FakeEngine {
        fn pcscf_reachable(&self, _runner: &dyn CommandRunner, pcscf: &str) -> bool {
            self.probed.borrow_mut().push(pcscf.to_string());
            match &*self.reachable_addrs.borrow() {
                Some(up) => up.iter().any(|a| a == pcscf),
                None => *self.pcscf_reachable.borrow(),
            }
        }

        fn is_tunnel_established(&self, _runner: &dyn CommandRunner) -> bool {
            *self.established.borrow()
        }
        fn latest_pcscf(&self, _runner: &dyn CommandRunner) -> Option<String> {
            self.pcscf.borrow().clone()
        }
        fn latest_pcscf_pool(&self, _runner: &dyn CommandRunner) -> Vec<String> {
            self.pool.borrow().clone()
        }
        fn is_process_alive(&self, _runner: &dyn CommandRunner) -> bool {
            *self.alive.borrow()
        }
        fn terminate(&self, _runner: &dyn CommandRunner) {
            *self.terminate_calls.borrow_mut() += 1;
            self.call_order.borrow_mut().push("terminate");
        }
        fn reinitiate(&self, _runner: &dyn CommandRunner) {
            *self.reinitiate_calls.borrow_mut() += 1;
            self.call_order.borrow_mut().push("reinitiate");
        }
        fn ike_sa_loss_cause(&self, _runner: &dyn CommandRunner) -> IkeSaLossCause {
            self.call_order.borrow_mut().push("loss_cause");
            *self.loss_cause.borrow()
        }
        fn inbound_esp(&self) -> Option<InboundEsp> {
            *self.inbound.borrow()
        }
        fn tun_rx_packets(&self) -> Option<u64> {
            *self.tun_rx.borrow()
        }
        fn route_ok(&self, _runner: &dyn CommandRunner) -> bool {
            *self.route_ok.borrow()
        }
        fn ike_header(&self) -> Option<(u32, IkeHeaderState)> {
            self.ike_header.borrow().clone()
        }
        fn ike_activity(&self, _runner: &dyn CommandRunner) -> Option<IkeActivity> {
            self.activity.borrow().clone()
        }
        fn ike_evidence_since(&self, _runner: &dyn CommandRunner, _mark: LogMark) -> IkeEvidence {
            *self.evidence.borrow()
        }
        fn send_dpd(&self, _runner: &dyn CommandRunner) -> bool {
            *self.dpd_calls.borrow_mut() += 1;
            self.call_order.borrow_mut().push("dpd");
            *self.dpd_accepted.borrow()
        }
        fn child_sa_rejection(&self, _runner: &dyn CommandRunner) -> Option<String> {
            self.call_order.borrow_mut().push("rejection");
            self.rejection.borrow().clone()
        }
        fn repair_default_route(&self, _runner: &dyn CommandRunner, _pcscf: &str) -> bool {
            *self.repair_calls.borrow_mut() += 1;
            if !*self.route_missing.borrow() {
                return false;
            }
            // Repaired: the route is back, and with it reachability. Modelling
            // that here is the point -- a missing route is a *cause* of
            // unreachability, so fixing it must clear the symptom too.
            *self.route_missing.borrow_mut() = false;
            *self.pcscf_reachable.borrow_mut() = true;
            *self.route_repairs.borrow_mut() += 1;
            self.call_order.borrow_mut().push("repair_route");
            true
        }
        fn steady_state_health(&self, _runner: &dyn CommandRunner) -> SteadyStateHealth {
            *self.health.borrow()
        }
        fn restart_process(&self, _runner: &dyn CommandRunner) {
            *self.restart_calls.borrow_mut() += 1;
        }
        fn recreate_interface(&self, _runner: &dyn CommandRunner) -> bool {
            *self.recreate_interface_calls.borrow_mut() += 1;
            self.call_order.borrow_mut().push("recreate_interface");
            self.recreate_ok
        }
        fn max_establish_attempts(&self) -> Option<u32> {
            self.max_attempts
        }
        fn reinitiate_cadence(&self) -> Option<u32> {
            self.reinitiate_cadence
        }
    }

    #[test]
    fn establishing_with_a_child_sa_but_no_pcscf_yet_keeps_waiting_and_marks_stuck() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            established: RefCell::new(true),
            pcscf: RefCell::new(None),
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        let outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(outcome, EstablishOutcome::StillEstablishing);
        assert!(
            stuck,
            "CHILD_SA established but no P-CSCF must set stuck_without_pcscf"
        );
    }

    #[test]
    fn establishing_with_child_sa_and_pcscf_transitions_to_established() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            established: RefCell::new(true),
            pcscf: RefCell::new(Some("10.0.0.1".to_string())),
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        let outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(
            outcome,
            EstablishOutcome::Established {
                pcscf: "10.0.0.1".to_string()
            }
        );
    }

    #[test]
    fn establishing_process_death_is_fatal_for_this_line() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            alive: RefCell::new(false),
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        assert_eq!(
            tick_establishing(&engine, &runner, &mut attempt, &mut stuck),
            EstablishOutcome::FatalProcessDied
        );
    }

    #[test]
    fn strongswan_reinitiates_every_15_attempts_while_still_waiting() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut attempt = 0;
        let mut stuck = false;
        for _ in 0..14 {
            tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        }
        assert_eq!(
            *engine.reinitiate_calls.borrow(),
            0,
            "no reinitiate before the 15th attempt"
        );
        tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(attempt, 15);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(
            *engine.terminate_calls.borrow(),
            0,
            "no terminate when not stuck-without-pcscf"
        );
    }

    #[test]
    fn strongswan_stuck_without_pcscf_terminates_before_reinitiating_at_the_cadence() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            established: RefCell::new(true), // CHILD_SA up, but pcscf stays None
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        for _ in 0..STRONGSWAN_REINITIATE_EVERY {
            tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        }
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
    }

    #[test]
    fn swu_never_reinitiates_in_place_only_counts_toward_its_own_timeout() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            max_attempts: Some(SWU_MAX_ESTABLISH_ATTEMPTS),
            reinitiate_cadence: None,
            ..Default::default()
        };
        let mut attempt = 0;
        let mut stuck = false;
        for _ in 0..SWU_MAX_ESTABLISH_ATTEMPTS - 1 {
            let outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
            assert_eq!(outcome, EstablishOutcome::StillEstablishing);
        }
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        let final_outcome = tick_establishing(&engine, &runner, &mut attempt, &mut stuck);
        assert_eq!(final_outcome, EstablishOutcome::FatalTimedOut);
    }

    #[test]
    fn steady_state_dead_process_is_restarted() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            alive: RefCell::new(false),
            ..Default::default()
        };
        let outcome =
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::ProcessDied,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.restart_calls.borrow(), 1);
    }

    #[test]
    fn steady_state_vici_broken_restarts_the_process() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ViciBroken),
            ..Default::default()
        };
        let outcome =
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::ViciBroken,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.restart_calls.borrow(), 1);
    }

    #[test]
    fn steady_state_tun_vanished_terminates_then_reinitiates_not_a_full_restart() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::TunVanished),
            ..Default::default()
        };
        let outcome =
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::TunVanished,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(
            *engine.restart_calls.borrow(),
            0,
            "must not be a full process restart"
        );
        assert_eq!(
            *engine.recreate_interface_calls.borrow(),
            1,
            "regression test: an earlier version of this port detected \
             TunVanished but never recreated the interface, so every \
             subsequent negotiation kept succeeding at the IKE/CHILD_SA \
             level while silently failing to install a working data path"
        );
    }

    #[test]
    fn steady_state_tun_vanished_recreates_before_it_terminates_and_reinitiates() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::TunVanished),
            ..Default::default()
        };
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            *engine.call_order.borrow(),
            vec!["recreate_interface", "terminate", "reinitiate"],
            "the original script's ordering: the interface must exist before \
             the renegotiation that installs a data path onto it"
        );
    }

    #[test]
    fn steady_state_tun_vanished_leaves_the_sa_alone_when_the_interface_cannot_be_recreated() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::TunVanished),
            recreate_ok: false,
            ..Default::default()
        };
        let outcome =
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::TunUnavailable,
                ike_sa_loss_cause: None,
            },
            "a distinct reason, so the caller can tell 'recreated it' from \
             'could not', and skip the agent restart for the latter"
        );
        assert!(!recovery_restarts_agent(DegradeReason::TunUnavailable));
        assert_eq!(*engine.recreate_interface_calls.borrow(), 1);
        assert_eq!(
            (
                *engine.terminate_calls.borrow(),
                *engine.reinitiate_calls.borrow()
            ),
            (0, 0),
            "measured live 2026-07-31: when a container is replaced, the \
             previous run's namespaces hold this line's if_id for ~2.5min and \
             nothing shortens that, so no interface can exist yet. Tearing the \
             SA down every 30s through that window only churns the carrier \
             (eight and six IKE_SA setups across two startups, against two and \
             two after this gate) and cannot produce a data path — the tick \
             after the id frees recovers normally"
        );
    }

    fn pool(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_dead_selection_with_a_live_pool_member_never_rebuilds_the_tunnel() {
        // specs/091 R9: the agent fails over to B; rebuilding the tunnel over
        // A would defeat that (US1: "without a tunnel rebuild").
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            reachable_addrs: RefCell::new(Some(pool(&["10.0.1.10"]))),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        let ticks = 2 * pcscf_unreachable_strikes(STEADY_STATE_POLL_INTERVAL);
        for _ in 0..ticks {
            let outcome = super::tick_steady_state_with_pool(
                &engine,
                &runner,
                "10.0.0.10",
                &pool(&["10.0.0.10", "10.0.1.10"]),
                &mut tracking,
                STEADY_STATE_POLL_INTERVAL,
                &|| false,
                &|| None,
            );
            assert_eq!(outcome, SteadyOutcome::StillUp);
        }
        assert_eq!(
            tracking.pcscf_via_sibling,
            Some("10.0.1.10".parse().unwrap())
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn a_wholly_dead_pool_still_rebuilds_the_tunnel() {
        // The rebuild is the spec's "rediscovery" (US3).
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            reachable_addrs: RefCell::new(Some(Vec::new())),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        let mut outcome = SteadyOutcome::StillUp;
        for _ in 0..pcscf_unreachable_strikes(STEADY_STATE_POLL_INTERVAL) {
            outcome = super::tick_steady_state_with_pool(
                &engine,
                &runner,
                "10.0.0.10",
                &pool(&["10.0.0.10", "10.0.1.10"]),
                &mut tracking,
                STEADY_STATE_POLL_INTERVAL,
                &|| false,
                &|| None,
            );
        }
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
                ike_sa_loss_cause: None,
            }
        );
    }

    #[test]
    fn a_healthy_selection_probes_no_other_pool_member() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        super::tick_steady_state_with_pool(
            &engine,
            &runner,
            "10.0.0.10",
            &pool(&["10.0.0.10", "10.0.1.10", "10.0.2.10"]),
            &mut SteadyTracking::default(),
            STEADY_STATE_POLL_INTERVAL,
            &|| false,
            &|| None,
        );
        assert_eq!(*engine.probed.borrow(), pool(&["10.0.0.10"]));
    }

    #[test]
    fn an_unreachable_pcscf_rebuilds_the_tunnel_after_three_strikes() {
        // The 2026-08-17 outage, as a test. Every structural check passes --
        // process alive, vici fine, interface present, CHILD_SA installed,
        // P-CSCF unchanged -- and nothing can be reached, because the route
        // through the tunnel was never reinstalled. That state lasted 8 hours
        // and the agent exited 4804 times, because no check asked whether the
        // data path carried traffic.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();

        // A single failure is routine on a mobile bearer -- one dropped packet
        // during a rekey must not tear down a working tunnel.
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 0, "no churn yet");

        // Sustained, though, means the data path is genuinely gone.
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
                ike_sa_loss_cause: None,
            }
        );
        // Same remedy as TunVanished, and in the same order: recreate the
        // interface first, then rebuild the negotiation onto it.
        assert_eq!(*engine.recreate_interface_calls.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(
            streak.pcscf_unreachable.count, 0,
            "the streak resets after acting"
        );
    }

    #[test]
    fn a_recovered_reachability_resets_the_streak_before_it_ever_acts() {
        // A flapping bearer must never accumulate its way to a teardown.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut streak = SteadyTracking::default();
        for _ in 0..10 {
            *engine.pcscf_reachable.borrow_mut() = false;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak);
            *engine.pcscf_reachable.borrow_mut() = true;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak);
        }
        assert_eq!(streak.pcscf_unreachable.count, 0);
        assert_eq!(
            *engine.reinitiate_calls.borrow(),
            0,
            "alternating reachability is not a sustained loss and must not rebuild"
        );
    }

    #[test]
    fn a_reachable_pcscf_is_still_up_and_touches_nothing() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut streak = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(*engine.restart_calls.borrow(), 0);
    }

    #[test]
    fn a_changed_pcscf_is_refreshed_rather_than_rebuilt_even_when_unreachable() {
        // Ordering matters: when the carrier moves the P-CSCF, the *old*
        // address going unreachable is expected. Refreshing to the new one is
        // the cheap correct remedy; rebuilding the tunnel would be churn. This
        // is the exact sequence that preceded the outage.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.2".to_string())),
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::PcscfChanged {
                new_pcscf: "10.0.0.2".to_string()
            }
        );
        assert_eq!(
            streak.pcscf_unreachable.count, 0,
            "a refresh must not count as an unreachable tick"
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
    }

    #[test]
    fn a_missing_default_route_is_repaired_without_touching_the_tunnel() {
        // The 2026-08-19 outage. A 2-minute WAN blip (a scheduled router reboot)
        // tore the CHILD_SA down; charon's `down-client` removed the carrier
        // address, and the kernel deleted the interface's default route with it.
        // The reconnect restored the address and nothing restored the route, so
        // every SIP connect got ENETUNREACH for six hours while every structural
        // check -- process alive, VICI fine, interface present, CHILD_SA
        // installed -- passed.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();

        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
                ike_sa_loss_cause: None,
            },
            "a missing route must be named as such, not inferred from unreachability"
        );
        assert_eq!(*engine.route_repairs.borrow(), 1);
        // The whole point: no renegotiation. A rebuild costs a dropped call and
        // -- as the outage proved -- cannot fix this anyway.
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(*engine.recreate_interface_calls.borrow(), 0);
        assert_eq!(
            streak.pcscf_unreachable.count, 0,
            "a repaired route must not count as a strike"
        );

        // And the line is well again on the next tick.
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
    }

    #[test]
    fn unreachable_with_the_route_present_still_escalates_to_a_rebuild() {
        // The route repair must not swallow the fault it does not explain --
        // otherwise it becomes a way to report success while the line stays dead.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            route_missing: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        let strikes = pcscf_unreachable_strikes(STEADY_STATE_POLL_INTERVAL);
        let mut rebuilt = false;
        for _ in 0..strikes {
            if let SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
                ..
            } = tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak)
            {
                rebuilt = true;
            }
        }
        assert!(rebuilt, "a genuinely dead data path must still be rebuilt");
        assert_eq!(*engine.route_repairs.borrow(), 0);
    }

    #[test]
    fn every_recovery_reason_has_the_wire_label_of_the_same_name() {
        use crate::control::protocol::TunnelRecoveryReason;
        let reasons = [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::TunUnavailable,
            DegradeReason::ChildSaMissing,
            DegradeReason::IkeSaLost,
            DegradeReason::ChildSaStuck,
            DegradeReason::ChildSaRekeyStuck,
            DegradeReason::PcscfChanged,
            DegradeReason::PcscfUnreachable,
            DegradeReason::DefaultRouteMissing,
            DegradeReason::PcscfPoolExhausted,
            DegradeReason::IkeSaNegotiationStuck,
        ];
        assert_eq!(reasons.len(), TunnelRecoveryReason::ALL.len());
        for reason in reasons {
            let wire = TunnelRecoveryReason::from(reason);
            assert_eq!(format!("{reason:?}"), format!("{wire:?}"));
        }
    }

    #[test]
    fn a_missing_route_is_repaired_even_while_the_pcscf_answers() {
        // #120 finding 3. The repair used to run only after the probe failed,
        // so with the probe removed (or answering over whatever else survived)
        // a lost route was never put back: run C left a line down for 10
        // minutes. The route is structural; it gets checked like the rest.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(true),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.route_repairs.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(*engine.recreate_interface_calls.borrow(), 0);
        assert_eq!(streak.pcscf_unreachable.count, 0);
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
    }

    #[test]
    fn the_route_is_checked_before_the_probe() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        assert!(matches!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
                ..
            }
        ));
        assert!(
            engine.probed.borrow().is_empty(),
            "a repairing tick must not depend on, or even run, the probe"
        );
    }

    #[test]
    fn an_empty_pcscf_skips_the_route_check() {
        // No P-CSCF means no family to check; checking both would invent a
        // missing IPv4 route on an IPv6-only line.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.repair_calls.borrow(), 0);
    }

    #[test]
    fn a_present_route_still_lets_the_probe_decide() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut streak = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.repair_calls.borrow(), 1);
        assert!(!engine.call_order.borrow().contains(&"repair_route"));
        assert_eq!(*engine.probed.borrow(), vec!["10.0.0.1".to_string()]);
    }

    #[test]
    fn structural_faults_and_pcscf_changes_skip_the_route_check() {
        let runner = MockCommandRunner::new();
        for health in [
            SteadyStateHealth::ViciBroken,
            SteadyStateHealth::TunVanished,
            SteadyStateHealth::IkeSaMissing,
            SteadyStateHealth::ChildSaMissing,
        ] {
            let engine = FakeEngine {
                health: RefCell::new(health),
                route_missing: RefCell::new(true),
                ..Default::default()
            };
            let outcome =
                tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
            assert!(
                !matches!(
                    outcome,
                    SteadyOutcome::Recovered {
                        reason: DegradeReason::DefaultRouteMissing,
                        ..
                    }
                ),
                "{health:?}: {outcome:?}"
            );
            assert_eq!(*engine.repair_calls.borrow(), 0, "{health:?}");
        }

        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.2".to_string())),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default()),
            SteadyOutcome::PcscfChanged {
                new_pcscf: "10.0.0.2".to_string()
            }
        );
        assert_eq!(*engine.repair_calls.borrow(), 0);
    }

    #[test]
    fn the_route_is_repaired_at_most_once_per_tick() {
        // Route present, P-CSCF silent: the unreachable branch must not ask
        // again what was asked milliseconds earlier in the same tick.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.repair_calls.borrow(), 1);
        assert_eq!(streak.pcscf_unreachable.count, 1);
    }

    #[test]
    fn the_repair_window_is_bounded_in_real_time_on_every_engine() {
        // Pins the outcome an operator cares about: how long a lost data path
        // can persist. This used to assert against `STEADY_STATE_POLL_INTERVAL`
        // only, which is the strongswan loop's cadence — so it passed while the
        // swu loop, polling six times faster, was acting after ~15s and tearing
        // down working tunnels on routine bearer blips.
        for interval in [
            STEADY_STATE_POLL_INTERVAL,
            SWU_STEADY_STATE_POLL_INTERVAL,
            Duration::from_secs(1),
        ] {
            let strikes = pcscf_unreachable_strikes(interval);
            let worst = interval * strikes;
            assert!(
                worst <= Duration::from_secs(120),
                "at a {interval:?} cadence a lost data path must be repaired in ~2 minutes, \
                 not {worst:?}"
            );
            assert!(
                worst >= PCSCF_UNREACHABLE_WINDOW,
                "at a {interval:?} cadence the tunnel must survive a blip shorter than \
                 {PCSCF_UNREACHABLE_WINDOW:?}, but it is torn down after {worst:?}"
            );
        }
    }

    #[test]
    fn the_unreachable_window_is_counted_in_time_when_the_cadence_changes() {
        // The strongswan loop ticks every 5s around a hold (specs/100). Three
        // failures there are 15s, not the 90s three are at 30s.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        let mut tick = |interval| {
            super::tick_steady_state(&engine, &runner, "10.0.0.1", &mut streak, interval)
        };
        let quick = Duration::from_secs(5);
        // 30s of quick ticks, then 30s ticks: rebuilt on the one reaching 90s.
        for n in 0..6 {
            assert_eq!(tick(quick), SteadyOutcome::StillUp, "quick tick {n}");
        }
        assert_eq!(tick(STEADY_STATE_POLL_INTERVAL), SteadyOutcome::StillUp);
        assert!(matches!(
            tick(STEADY_STATE_POLL_INTERVAL),
            SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
                ..
            }
        ));
        assert_eq!(streak.pcscf_unreachable, Streak::default());
    }

    #[test]
    fn a_missing_child_sa_gets_one_reinitiate_and_a_minute_at_any_cadence() {
        // At 30s: re-initiate, then rebuild on the next tick (as ever). At the
        // 5s cadence around a hold: one re-initiate, quiet waiting, and the
        // rebuild once the same minute is covered.
        let runner = MockCommandRunner::new();
        for (interval, waits) in [
            (STEADY_STATE_POLL_INTERVAL, 0),
            (Duration::from_secs(5), 10),
        ] {
            let engine = FakeEngine {
                health: RefCell::new(SteadyStateHealth::ChildSaMissing),
                ..Default::default()
            };
            let mut tracking = SteadyTracking::default();
            let mut tick =
                || super::tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking, interval);
            assert!(matches!(
                tick(),
                SteadyOutcome::Recovered {
                    reason: DegradeReason::ChildSaMissing,
                    ..
                }
            ));
            for n in 0..waits {
                assert_eq!(
                    tick(),
                    SteadyOutcome::WaitingForNegotiation,
                    "{interval:?} wait {n}"
                );
            }
            assert!(
                matches!(
                    tick(),
                    SteadyOutcome::Recovered {
                        reason: DegradeReason::ChildSaStuck,
                        ..
                    }
                ),
                "{interval:?}"
            );
            assert_eq!(*engine.reinitiate_calls.borrow(), 2, "{interval:?}");
            assert_eq!(*engine.terminate_calls.borrow(), 1, "{interval:?}");
        }
    }

    #[test]
    fn a_slow_poll_still_needs_more_than_one_failure() {
        // A window alone would let an arbitrarily slow loop act on a single
        // sample, and one failed TCP connect on a mobile bearer is routine.
        let strikes = pcscf_unreachable_strikes(Duration::from_secs(600));
        assert_eq!(strikes, PCSCF_UNREACHABLE_MIN_STRIKES);
        assert!(strikes > 1);
        // A zero interval must not divide by zero.
        assert!(pcscf_unreachable_strikes(Duration::ZERO) >= PCSCF_UNREACHABLE_MIN_STRIKES);
    }

    #[test]
    fn the_swu_cadence_does_not_rebuild_on_a_short_blip() {
        // The concrete regression: 15s of unreachability used to be enough on
        // swu. It must not be.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut streak = SteadyTracking::default();
        let blip_ticks =
            (Duration::from_secs(15).as_secs() / SWU_STEADY_STATE_POLL_INTERVAL.as_secs()) as u32;
        for tick in 0..blip_ticks {
            assert_eq!(
                super::tick_steady_state(
                    &engine,
                    &runner,
                    "10.0.0.1",
                    &mut streak,
                    SWU_STEADY_STATE_POLL_INTERVAL,
                ),
                SteadyOutcome::StillUp,
                "tick {tick} of a 15s blip must not rebuild the tunnel"
            );
        }
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);

        // ...but a genuinely lost data path is still repaired, within the window.
        let remaining = pcscf_unreachable_strikes(SWU_STEADY_STATE_POLL_INTERVAL) - blip_ticks;
        let mut rebuilt = false;
        for _ in 0..remaining {
            if let SteadyOutcome::Recovered {
                reason: DegradeReason::PcscfUnreachable,
                ..
            } = super::tick_steady_state(
                &engine,
                &runner,
                "10.0.0.1",
                &mut streak,
                SWU_STEADY_STATE_POLL_INTERVAL,
            ) {
                rebuilt = true;
            }
        }
        assert!(rebuilt, "a sustained loss must still be repaired");
    }

    #[test]
    fn steady_state_child_sa_missing_only_reinitiates_no_terminate_no_restart() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let outcome =
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.restart_calls.borrow(), 0);
    }

    fn child_sa_tick(
        engine: &FakeEngine,
        runner: &MockCommandRunner,
        tracking: &mut SteadyTracking,
    ) -> SteadyOutcome {
        tick_steady_state(engine, runner, "10.0.0.1", tracking)
    }

    #[test]
    fn rebuild_window_latches_on_the_third_rebuild_within_the_window() {
        use std::time::Instant;
        let w = Duration::from_secs(300);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut r = RebuildWindow::new(w);
        assert!(!r.record(at(0)));
        assert!(!r.record(at(10)));
        assert!(!r.is_flapping());
        assert!(
            r.record(at(20)),
            "the third latches, and reports the change"
        );
        assert!(r.is_flapping());
        assert!(!r.record(at(30)), "a fourth stays latched, no change");
        // Not yet a full window since the newest rebuild.
        assert!(!r.tick(at(30 + 299)));
        assert!(r.is_flapping());
        assert!(r.tick(at(30 + 300)), "a full quiet window unlatches");
        assert!(!r.is_flapping());
    }

    #[test]
    fn rebuilds_spread_wider_than_the_window_never_latch() {
        use std::time::Instant;
        let t0 = Instant::now();
        let mut r = RebuildWindow::new(Duration::from_secs(300));
        for i in 0..6 {
            assert!(!r.record(t0 + Duration::from_secs(i * 200)));
        }
        assert!(!r.is_flapping());
    }

    #[test]
    fn a_missing_ike_sa_reinitiates_without_terminate_and_always_restarts_the_agent() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking {
            child_sa_missing: Streak {
                count: 1,
                covered: STEADY_STATE_POLL_INTERVAL,
            },
            child_sa_usable: true,
            ..Default::default()
        };
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaLost,
                ike_sa_loss_cause: Some(IkeSaLossCause::Unknown),
            }
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(tracking.child_sa_missing.count, 0);
        assert!(!tracking.child_sa_usable);
    }

    #[test]
    fn an_ike_sa_loss_between_two_child_sa_misses_resets_the_strike_count() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        child_sa_tick(&engine, &runner, &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::IkeSaMissing;
        child_sa_tick(&engine, &runner, &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::ChildSaMissing;
        let outcome = child_sa_tick(&engine, &runner, &mut tracking);
        assert!(
            matches!(
                outcome,
                SteadyOutcome::Recovered {
                    reason: DegradeReason::ChildSaMissing,
                    ..
                }
            ),
            "the second miss after an IKE_SA loss is a first strike: {outcome:?}"
        );
    }

    #[test]
    fn a_route_repair_restarts_the_agent_but_does_not_renegotiate() {
        assert!(recovery_restarts_agent(DegradeReason::DefaultRouteMissing));
        assert!(!recovery_renegotiates(DegradeReason::DefaultRouteMissing));
        assert!(!recovery_renegotiates(DegradeReason::ChildSaMissing));
        for r in [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::ChildSaStuck,
            DegradeReason::ChildSaRekeyStuck,
            DegradeReason::IkeSaLost,
            DegradeReason::PcscfUnreachable,
            DegradeReason::PcscfPoolExhausted,
        ] {
            assert!(recovery_restarts_agent(r), "{r:?}");
            assert!(recovery_renegotiates(r), "{r:?}");
        }
    }

    // --- specs/097: the agent's verdict on its P-CSCF pool ----------------

    use crate::ims::pcscf_files::{PoolState, PoolStateMark, POOL_STATE_VERSION};

    /// A wall-clock instant comfortably in the past, so `now()` is after it.
    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A mark the agent set at `marked_at` that ends an hour after now.
    fn agent_mark(addr: &str, cause: &str, marked_at: SystemTime) -> PoolStateMark {
        PoolStateMark {
            addr: addr.parse().unwrap(),
            until_unix: unix_secs(SystemTime::now()) + 3600,
            failures: 1,
            cause: cause.into(),
            marked_at_unix: unix_secs(marked_at),
        }
    }

    fn state(marks: Vec<PoolStateMark>) -> PoolState {
        PoolState {
            version: POOL_STATE_VERSION,
            last_registered: None,
            marks,
        }
    }

    const BASELINE: u64 = 1_790_000_000;

    #[test]
    fn exhausted_pool_needs_every_member_freshly_marked() {
        let base = Some(at(BASELINE));
        let now = SystemTime::now();
        let fresh = |a: &str| agent_mark(a, "gm-keepalive", at(BASELINE + 10));
        let one = state(vec![fresh("10.0.0.11")]);
        let got = exhausted_pool(&pool(&["10.0.0.11"]), "10.0.0.11", Some(&one), base, now)
            .expect("a single freshly marked P-CSCF is an exhausted pool");
        assert_eq!(got[0].addr, "10.0.0.11");
        assert_eq!(got[0].cause, "gm-keepalive");

        // Nothing known means nothing to act on.
        assert_eq!(
            exhausted_pool(&pool(&["10.0.0.11"]), "", None, base, now),
            None
        );
        assert_eq!(
            exhausted_pool(&pool(&["10.0.0.11"]), "", Some(&one), None, now),
            None
        );
        assert_eq!(exhausted_pool(&[], "", Some(&one), base, now), None);
        // A pool address that does not parse cannot be judged.
        assert_eq!(
            exhausted_pool(&pool(&["bogus"]), "", Some(&one), base, now),
            None
        );
    }

    #[test]
    fn a_mark_counts_only_if_fresh_unexpired_and_not_retry_after() {
        let base = Some(at(BASELINE));
        let now = SystemTime::now();
        let p = pool(&["10.0.0.11"]);
        let check = |m: PoolStateMark| exhausted_pool(&p, "", Some(&state(vec![m])), base, now);

        // Set before the negotiation: it outlived the rebuild it caused.
        assert_eq!(
            check(agent_mark("10.0.0.11", "transport", at(BASELINE - 1))),
            None
        );
        // From a file written before the marking time existed.
        let mut old = agent_mark("10.0.0.11", "transport", at(BASELINE + 1));
        old.marked_at_unix = 0;
        assert_eq!(check(old), None);
        // Expired: the agent itself would try the address again.
        let mut expired = agent_mark("10.0.0.11", "transport", at(BASELINE + 1));
        expired.until_unix = unix_secs(now) - 1;
        assert_eq!(check(expired), None);
        // A requested pause is not a failure.
        assert_eq!(
            check(agent_mark("10.0.0.11", "retry-after", at(BASELINE + 1))),
            None
        );
        // Set in the very second of the negotiation still counts.
        assert!(check(agent_mark("10.0.0.11", "transport", at(BASELINE))).is_some());
        for cause in [
            "gm-keepalive",
            "register-timeout",
            "transport",
            "endpoint-failure:subscribe-timeout",
        ] {
            assert!(
                check(agent_mark("10.0.0.11", cause, at(BASELINE + 1))).is_some(),
                "{cause}"
            );
        }
    }

    #[test]
    fn exhausted_pool_falls_back_to_the_selection_and_matches_addresses_not_text() {
        let base = Some(at(BASELINE));
        let now = SystemTime::now();
        let v6 = state(vec![agent_mark(
            "2001:db8::1",
            "transport",
            at(BASELINE + 1),
        )]);
        // No pool published: the selection alone.
        assert!(exhausted_pool(&[], "2001:0db8::1", Some(&v6), base, now).is_some());
        assert!(exhausted_pool(&pool(&["2001:0db8:0::1"]), "", Some(&v6), base, now).is_some());
        // A mark on an address this line was never given is irrelevant.
        let stray = state(vec![agent_mark("10.0.9.9", "transport", at(BASELINE + 1))]);
        assert_eq!(
            exhausted_pool(&pool(&["10.0.0.11"]), "", Some(&stray), base, now),
            None
        );
    }

    /// One strongswan-cadence tick of a line whose agent reports `agent`.
    fn pool_tick(
        engine: &FakeEngine,
        current_pool: &[String],
        tracking: &mut SteadyTracking,
        in_call: bool,
        agent: &PoolState,
    ) -> SteadyOutcome {
        super::tick_steady_state_with_pool(
            engine,
            &MockCommandRunner::new(),
            &current_pool[0],
            current_pool,
            tracking,
            STEADY_STATE_POLL_INTERVAL,
            &|| in_call,
            &|| Some(agent.clone()),
        )
    }

    fn negotiated() -> SteadyTracking {
        SteadyTracking {
            negotiated_at: Some(at(BASELINE)),
            ..Default::default()
        }
    }

    const POOL_EXHAUSTED: SteadyOutcome = SteadyOutcome::Recovered {
        reason: DegradeReason::PcscfPoolExhausted,
        ike_sa_loss_cause: None,
    };

    #[test]
    fn a_single_exhausted_pcscf_rebuilds_the_tunnel() {
        // #120 finding 4: Vodafone's only P-CSCF dead, the tunnel fine.
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        let agent = state(vec![agent_mark(
            "10.0.0.11",
            "gm-keepalive",
            at(BASELINE + 5),
        )]);
        let outcome = pool_tick(&engine, &pool(&["10.0.0.11"]), &mut tracking, false, &agent);
        assert_eq!(outcome, POOL_EXHAUSTED);
        assert_eq!(
            *engine.call_order.borrow(),
            vec!["recreate_interface", "terminate", "reinitiate"]
        );
        assert_eq!(tracking.pool_exhausted.len(), 1);
        assert_eq!(tracking.pool_exhausted[0].addr, "10.0.0.11");
    }

    #[test]
    fn the_agent_verdict_wins_over_a_passing_probe() {
        // The probe knocks on the unprotected port; the agent signals over the
        // protected one. When the agent says the P-CSCF is dead, it is.
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(true),
            ..Default::default()
        };
        let agent = state(vec![agent_mark(
            "10.0.0.11",
            "register-timeout",
            at(BASELINE + 5),
        )]);
        let outcome = pool_tick(
            &engine,
            &pool(&["10.0.0.11"]),
            &mut negotiated(),
            false,
            &agent,
        );
        assert_eq!(outcome, POOL_EXHAUSTED);
        assert!(
            engine.probed.borrow().is_empty(),
            "decided before the probe"
        );
    }

    #[test]
    fn a_missing_route_is_repaired_before_the_pool_is_judged() {
        let engine = FakeEngine {
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        let outcome = pool_tick(
            &engine,
            &pool(&["10.0.0.11"]),
            &mut negotiated(),
            false,
            &agent,
        );
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn an_exhausted_pool_whose_interface_cannot_be_recreated_is_tun_unavailable() {
        let engine = FakeEngine {
            recreate_ok: false,
            ..Default::default()
        };
        let mut tracking = negotiated();
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        let outcome = pool_tick(&engine, &pool(&["10.0.0.11"]), &mut tracking, false, &agent);
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::TunUnavailable,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert!(tracking.pool_exhausted.is_empty(), "no rebuild was done");
    }

    #[test]
    fn the_exhaustion_rebuild_resets_the_unreachable_strikes() {
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        tracking.pcscf_unreachable = Streak {
            count: 2,
            covered: STEADY_STATE_POLL_INTERVAL * 2,
        };
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        pool_tick(&engine, &pool(&["10.0.0.11"]), &mut tracking, false, &agent);
        assert_eq!(tracking.pcscf_unreachable.count, 0);
    }

    #[test]
    fn stale_marks_from_before_the_negotiation_never_rebuild() {
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        let agent = state(vec![agent_mark(
            "10.0.0.11",
            "transport",
            at(BASELINE - 60),
        )]);
        let outcome = pool_tick(&engine, &pool(&["10.0.0.11"]), &mut tracking, false, &agent);
        assert_eq!(outcome, SteadyOutcome::StillUp);
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn an_exhausted_pool_waits_for_the_call_to_end() {
        // Clarification Q1: no cap, rebuild on the first tick after the call.
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        let p = pool(&["10.0.0.11"]);
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        for _ in 0..3 {
            assert_eq!(
                pool_tick(&engine, &p, &mut tracking, true, &agent),
                SteadyOutcome::StillUp
            );
            assert!(tracking.pool_exhausted_deferral_logged);
        }
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(
            pool_tick(&engine, &p, &mut tracking, false, &agent),
            POOL_EXHAUSTED
        );
        assert!(!tracking.pool_exhausted_deferral_logged);
    }

    #[test]
    fn the_deferral_note_clears_when_the_pool_recovers() {
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        let p = pool(&["10.0.0.11"]);
        let dead = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        pool_tick(&engine, &p, &mut tracking, true, &dead);
        assert!(tracking.pool_exhausted_deferral_logged);
        pool_tick(&engine, &p, &mut tracking, true, &state(vec![]));
        assert!(!tracking.pool_exhausted_deferral_logged);
    }

    // US2: a carrier with several P-CSCFs keeps failing over inside the agent.

    #[test]
    fn a_partly_marked_pool_never_rebuilds() {
        let p = pool(&["10.0.0.11", "10.0.0.12", "10.0.0.13", "10.0.0.14"]);
        for marked in 1..p.len() {
            for probe_answers in [true, false] {
                let engine = FakeEngine {
                    pcscf_reachable: RefCell::new(probe_answers),
                    ..Default::default()
                };
                let agent = state(
                    p[..marked]
                        .iter()
                        .map(|a| agent_mark(a, "gm-keepalive", at(BASELINE + 5)))
                        .collect(),
                );
                let outcome = pool_tick(&engine, &p, &mut negotiated(), false, &agent);
                assert_ne!(outcome, POOL_EXHAUSTED, "{marked} of 4 marked");
            }
        }
    }

    #[test]
    fn a_wholly_marked_pool_rebuilds() {
        let p = pool(&["10.0.0.11", "10.0.0.12", "10.0.0.13", "10.0.0.14"]);
        let engine = FakeEngine::default();
        let agent = state(
            p.iter()
                .map(|a| agent_mark(a, "register-timeout", at(BASELINE + 5)))
                .collect(),
        );
        let mut tracking = negotiated();
        assert_eq!(
            pool_tick(&engine, &p, &mut tracking, false, &agent),
            POOL_EXHAUSTED
        );
        assert_eq!(tracking.pool_exhausted.len(), 4);
    }

    #[test]
    fn a_member_cleared_by_a_registration_counts_as_unmarked() {
        // `on_registered` removes the mark; the state then simply lacks it.
        let p = pool(&["10.0.0.11", "10.0.0.12"]);
        let engine = FakeEngine::default();
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        let outcome = pool_tick(&engine, &p, &mut negotiated(), false, &agent);
        assert_eq!(outcome, SteadyOutcome::StillUp);
    }

    // US3: everything else in the tick comes first, as before.

    #[test]
    fn structural_faults_and_pcscf_changes_skip_the_exhaustion_check() {
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        let p = pool(&["10.0.0.11"]);
        for health in [
            SteadyStateHealth::ViciBroken,
            SteadyStateHealth::TunVanished,
            SteadyStateHealth::IkeSaMissing,
            SteadyStateHealth::ChildSaMissing,
        ] {
            let engine = FakeEngine {
                health: RefCell::new(health),
                ..Default::default()
            };
            let outcome = pool_tick(&engine, &p, &mut negotiated(), false, &agent);
            assert_ne!(outcome, POOL_EXHAUSTED, "{health:?}");
        }
        let dead = FakeEngine {
            alive: RefCell::new(false),
            ..Default::default()
        };
        let outcome = pool_tick(&dead, &p, &mut negotiated(), false, &agent);
        assert_ne!(outcome, POOL_EXHAUSTED);
        let moved = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.99".into())),
            ..Default::default()
        };
        assert_eq!(
            pool_tick(&moved, &p, &mut negotiated(), false, &agent),
            SteadyOutcome::PcscfChanged {
                new_pcscf: "10.0.0.99".into()
            }
        );
    }

    #[test]
    fn no_pool_state_leaves_the_tick_as_it_was() {
        let p = pool(&["10.0.0.11"]);
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        let outcome = super::tick_steady_state_with_pool(
            &engine,
            &MockCommandRunner::new(),
            "10.0.0.11",
            &p,
            &mut tracking,
            STEADY_STATE_POLL_INTERVAL,
            &|| false,
            &|| None,
        );
        assert_eq!(outcome, SteadyOutcome::StillUp);
        assert_eq!(*engine.probed.borrow(), vec!["10.0.0.11".to_string()]);
    }

    #[test]
    fn the_exhausted_members_describe_only_the_tick_that_rebuilt() {
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        let p = pool(&["10.0.0.11"]);
        let agent = state(vec![agent_mark("10.0.0.11", "transport", at(BASELINE + 5))]);
        pool_tick(&engine, &p, &mut tracking, false, &agent);
        assert!(!tracking.pool_exhausted.is_empty());
        pool_tick(&engine, &p, &mut tracking, false, &state(vec![]));
        assert!(tracking.pool_exhausted.is_empty());
    }

    #[test]
    fn the_loss_cause_is_read_before_the_replacement_negotiation_starts() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            ..Default::default()
        };
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            *engine.call_order.borrow(),
            vec!["loss_cause", "reinitiate"],
            "a new establishment would reset the parser"
        );
    }

    fn esp_seen(age_secs: u64) -> Option<InboundEsp> {
        Some(InboundEsp {
            last_in_age: Some(Duration::from_secs(age_secs)),
            packets: 7,
            tun_rx_packets: Some(500),
        })
    }

    #[test]
    fn a_healthy_tick_snapshots_inbound_esp() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            inbound: RefCell::new(esp_seen(4)),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(tracking.inbound.map(|(_, e)| e), esp_seen(4));
    }

    #[test]
    fn a_dead_peer_loss_after_recent_esp_is_classified_false() {
        // #120 finding 2: ESP still arriving while charon gave up.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            inbound: RefCell::new(esp_seen(30)),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::IkeSaMissing;
        *engine.loss_cause.borrow_mut() = IkeSaLossCause::DeadPeer;
        let outcome = tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(
            outcome,
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaLost,
                ike_sa_loss_cause: Some(IkeSaLossCause::DeadPeer),
            }
        );
        let c = tracking.dead_peer.expect("classified");
        assert_eq!(c.verdict, DeadPeerVerdict::False);
        assert_eq!(c.packets, Some(7));
        assert_eq!(tracking.inbound, None, "the lost SA's snapshot is spent");
    }

    #[test]
    fn a_dead_peer_loss_after_long_silence_is_classified_real() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            inbound: RefCell::new(esp_seen(300)),
            tun_rx: RefCell::new(Some(500)),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::IkeSaMissing;
        *engine.loss_cause.borrow_mut() = IkeSaLossCause::DeadPeer;
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(
            tracking.dead_peer.map(|c| c.verdict),
            Some(DeadPeerVerdict::Real)
        );
    }

    #[test]
    fn a_dead_peer_loss_after_long_silence_but_esp_since_is_classified_false() {
        // The path came back after the last healthy tick, before charon gave
        // up: the loss tick's counter is ahead of the snapshot's.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            inbound: RefCell::new(esp_seen(300)),
            tun_rx: RefCell::new(Some(500)),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::IkeSaMissing;
        *engine.loss_cause.borrow_mut() = IkeSaLossCause::DeadPeer;
        *engine.tun_rx.borrow_mut() = Some(517);
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        let c = tracking.dead_peer.expect("classified");
        assert_eq!(c.verdict, DeadPeerVerdict::False);
        assert_eq!(c.rx_since_snapshot, Some(17));
    }

    #[test]
    fn a_dead_peer_loss_with_no_snapshot_is_unknown() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            loss_cause: RefCell::new(IkeSaLossCause::DeadPeer),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(
            tracking.dead_peer.map(|c| c.verdict),
            Some(DeadPeerVerdict::Unknown)
        );
    }

    #[test]
    fn other_loss_causes_and_outcomes_are_not_classified() {
        let runner = MockCommandRunner::new();
        for cause in [IkeSaLossCause::CarrierDelete, IkeSaLossCause::Unknown] {
            let engine = FakeEngine {
                inbound: RefCell::new(esp_seen(30)),
                ..Default::default()
            };
            let mut tracking = SteadyTracking::default();
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
            *engine.health.borrow_mut() = SteadyStateHealth::IkeSaMissing;
            *engine.loss_cause.borrow_mut() = cause;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
            assert_eq!(tracking.dead_peer, None, "{cause}");
        }
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            loss_cause: RefCell::new(IkeSaLossCause::DeadPeer),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(tracking.dead_peer, None);
    }

    #[test]
    fn the_verdict_lasts_one_tick_and_unhealthy_ticks_drop_the_snapshot() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            loss_cause: RefCell::new(IkeSaLossCause::DeadPeer),
            inbound: RefCell::new(esp_seen(5)),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert!(tracking.dead_peer.is_some());
        *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(tracking.dead_peer, None);
        assert!(tracking.inbound.is_some());
        for health in [
            SteadyStateHealth::ViciBroken,
            SteadyStateHealth::TunVanished,
            SteadyStateHealth::ChildSaMissing,
        ] {
            *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
            *engine.health.borrow_mut() = health;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
            assert_eq!(tracking.inbound, None, "{health:?}");
        }
        *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        *engine.alive.borrow_mut() = false;
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(tracking.inbound, None, "ProcessDied");
    }

    #[test]
    fn ike_sa_lost_restarts_the_agent() {
        assert!(recovery_restarts_agent(DegradeReason::IkeSaLost));
    }

    #[test]
    fn a_second_consecutive_missing_child_sa_rebuilds_the_ike_sa() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            child_sa_tick(&engine, &runner, &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(
            child_sa_tick(&engine, &runner, &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaStuck,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 2);
        assert_eq!(tracking.child_sa_missing.count, 0);
    }

    #[test]
    fn a_healthy_tick_between_misses_prevents_escalation() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        child_sa_tick(&engine, &runner, &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
        child_sa_tick(&engine, &runner, &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::ChildSaMissing;
        assert_eq!(
            child_sa_tick(&engine, &runner, &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn repeated_child_sa_loss_alternates_with_no_backoff() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        let reasons: Vec<_> = (0..4)
            .map(|_| match child_sa_tick(&engine, &runner, &mut tracking) {
                SteadyOutcome::Recovered { reason, .. } => reason,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            reasons,
            [
                DegradeReason::ChildSaMissing,
                DegradeReason::ChildSaStuck,
                DegradeReason::ChildSaMissing,
                DegradeReason::ChildSaStuck
            ]
        );
    }

    #[test]
    fn a_missing_child_sa_never_reaches_the_route_repair() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            pcscf_reachable: RefCell::new(false),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        child_sa_tick(&engine, &runner, &mut tracking);
        child_sa_tick(&engine, &runner, &mut tracking);
        assert_eq!(*engine.route_repairs.borrow(), 0);
    }

    #[test]
    fn a_process_death_between_misses_resets_the_miss_count() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        child_sa_tick(&engine, &runner, &mut tracking);
        *engine.alive.borrow_mut() = false;
        child_sa_tick(&engine, &runner, &mut tracking);
        *engine.alive.borrow_mut() = true;
        assert_eq!(
            child_sa_tick(&engine, &runner, &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            },
            "the respawned connection gets its own first retry"
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn a_vanished_tun_resets_the_child_sa_miss_count() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        child_sa_tick(&engine, &runner, &mut tracking);
        assert_eq!(tracking.child_sa_missing.count, 1);
        *engine.health.borrow_mut() = SteadyStateHealth::TunVanished;
        child_sa_tick(&engine, &runner, &mut tracking);
        assert_eq!(tracking.child_sa_missing.count, 0);
    }

    #[test]
    fn child_sa_usable_follows_the_structural_health_check() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert!(tracking.child_sa_usable);

        for health in [
            SteadyStateHealth::ChildSaMissing,
            SteadyStateHealth::TunVanished,
            SteadyStateHealth::ViciBroken,
        ] {
            *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
            assert!(tracking.child_sa_usable);
            *engine.health.borrow_mut() = health;
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
            assert!(!tracking.child_sa_usable, "{health:?}");
        }

        *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        *engine.alive.borrow_mut() = false;
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert!(!tracking.child_sa_usable, "process died");
    }

    #[test]
    fn an_unreachable_pcscf_does_not_clear_child_sa_usable() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert!(tracking.child_sa_usable);
    }

    #[test]
    fn steady_state_pcscf_change_is_reported_without_any_recovery_action() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.9".to_string())),
            ..Default::default()
        };
        let outcome =
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default());
        assert_eq!(
            outcome,
            SteadyOutcome::PcscfChanged {
                new_pcscf: "10.0.0.9".to_string()
            }
        );
        assert_eq!(*engine.restart_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
    }

    #[test]
    fn steady_state_unchanged_pcscf_and_healthy_process_is_a_no_op_tick() {
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            pcscf: RefCell::new(Some("10.0.0.1".to_string())),
            ..Default::default()
        };
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut SteadyTracking::default()),
            SteadyOutcome::StillUp
        );
    }

    #[test]
    fn only_child_sa_missing_and_tun_unavailable_skip_the_agent_restart() {
        for reason in [DegradeReason::ChildSaMissing, DegradeReason::TunUnavailable] {
            assert!(
                !recovery_restarts_agent(reason),
                "{reason:?} should leave the agent alone"
            );
        }
        for reason in [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::PcscfChanged,
            DegradeReason::ChildSaStuck,
            DegradeReason::IkeSaLost,
        ] {
            assert!(
                recovery_restarts_agent(reason),
                "{reason:?} should restart the agent"
            );
        }
        // Both arms above list their variants literally, so a new one added to
        // DegradeReason would be silently untested. This match has no wildcard
        // and so fails to compile until whoever adds it decides which arm it
        // belongs in.
        for reason in [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::TunUnavailable,
            DegradeReason::ChildSaMissing,
            DegradeReason::PcscfChanged,
            DegradeReason::PcscfUnreachable,
            DegradeReason::DefaultRouteMissing,
            DegradeReason::ChildSaStuck,
            DegradeReason::ChildSaRekeyStuck,
            DegradeReason::IkeSaLost,
            DegradeReason::PcscfPoolExhausted,
            DegradeReason::IkeSaNegotiationStuck,
        ] {
            match reason {
                DegradeReason::ProcessDied
                | DegradeReason::ViciBroken
                | DegradeReason::TunVanished
                | DegradeReason::PcscfChanged
                // A fresh IKE_SA brings a new VIP/P-CSCF pool, so the agent
                // must re-register.
                | DegradeReason::ChildSaStuck
                // The same rebuild, done before the hard expiry (specs/093).
                | DegradeReason::ChildSaRekeyStuck
                // Likewise: a new IKE_SA, a new VIP and P-CSCF pool.
                | DegradeReason::IkeSaLost
                // A rebuilt tunnel means a new data path, which the agent has
                // to re-register over -- so it must be restarted.
                | DegradeReason::PcscfUnreachable
                // The same rebuild, decided from the agent's own verdict.
                | DegradeReason::PcscfPoolExhausted
                // A new IKE_SA, a new VIP and P-CSCF pool (specs/100).
                | DegradeReason::IkeSaNegotiationStuck
                // Restarted despite the repair itself being non-disruptive:
                // with no route there was no data path, so the agent's
                // registration and any call over it were already dead. Nothing
                // is lost by restarting, and its cached socket is useless.
                | DegradeReason::DefaultRouteMissing => assert!(recovery_restarts_agent(reason)),
                DegradeReason::TunUnavailable | DegradeReason::ChildSaMissing => {
                    assert!(!recovery_restarts_agent(reason))
                }
            }
        }
    }

    // --- specs/093: stuck CHILD_SA rekey ---------------------------------

    fn lifetime(unique_id: u32, expires_in: i64) -> SteadyStateHealth {
        SteadyStateHealth::Ok(Some(ChildSaLifetime {
            unique_id,
            expires_in: Some(expires_in),
        }))
    }

    const REKEY_STUCK: SteadyOutcome = SteadyOutcome::Recovered {
        reason: DegradeReason::ChildSaRekeyStuck,
        ike_sa_loss_cause: None,
    };

    /// One strongswan-cadence tick with `health` as the engine's report.
    fn rekey_tick(
        engine: &FakeEngine,
        health: SteadyStateHealth,
        tracking: &mut SteadyTracking,
    ) -> SteadyOutcome {
        rekey_tick_in_call(engine, health, tracking, &|| false)
    }

    fn rekey_tick_in_call(
        engine: &FakeEngine,
        health: SteadyStateHealth,
        tracking: &mut SteadyTracking,
        call_active: &dyn Fn() -> bool,
    ) -> SteadyOutcome {
        *engine.health.borrow_mut() = health;
        super::tick_steady_state_with_pool(
            engine,
            &MockCommandRunner::new(),
            "10.0.0.1",
            &[],
            tracking,
            STEADY_STATE_POLL_INTERVAL,
            call_active,
            &|| None,
        )
    }

    #[test]
    fn rekey_stuck_defers_during_a_call() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        for expires in [290, 260] {
            assert_eq!(
                rekey_tick_in_call(&engine, lifetime(12, expires), &mut tracking, &|| true),
                SteadyOutcome::StillUp
            );
            assert_eq!(tracking.rekey_stuck_deferral_logged, Some(12));
        }
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn rekey_stuck_rebuilds_when_the_call_ends() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick_in_call(&engine, lifetime(12, 290), &mut tracking, &|| true),
            SteadyOutcome::StillUp
        );
        assert_eq!(
            rekey_tick_in_call(&engine, lifetime(12, 260), &mut tracking, &|| false),
            REKEY_STUCK
        );
        assert_eq!(*engine.terminate_calls.borrow(), 1);
    }

    #[test]
    fn rekey_stuck_forced_under_margin() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        let mut expires = 290;
        while expires > REKEY_STUCK_FORCE_MARGIN.as_secs() as i64 {
            assert_eq!(
                rekey_tick_in_call(&engine, lifetime(12, expires), &mut tracking, &|| true),
                SteadyOutcome::StillUp,
                "{expires}"
            );
            expires -= 30;
        }
        assert!(expires > 0, "the forced rebuild still has {expires}s");
        assert_eq!(
            rekey_tick_in_call(&engine, lifetime(12, expires), &mut tracking, &|| true),
            REKEY_STUCK
        );
        assert_eq!(*engine.terminate_calls.borrow(), 1);
    }

    #[test]
    fn rekey_stuck_call_state_read_only_when_stuck() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        let reads = std::cell::Cell::new(0);
        let call_active = || {
            reads.set(reads.get() + 1);
            false
        };
        rekey_tick_in_call(&engine, lifetime(12, 3000), &mut tracking, &call_active);
        rekey_tick_in_call(&engine, lifetime(12, 301), &mut tracking, &call_active);
        assert_eq!(reads.get(), 0);
        rekey_tick_in_call(&engine, lifetime(12, 290), &mut tracking, &call_active);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn rekey_stuck_rebuilds_at_threshold() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 300), &mut tracking),
            REKEY_STUCK
        );
        assert_eq!(
            *engine.call_order.borrow(),
            vec!["rejection", "terminate", "reinitiate"]
        );
        assert_eq!(tracking.rekey_stuck_rebuilt, Some(12));
    }

    #[test]
    fn rekey_stuck_above_threshold_is_still_up() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 301), &mut tracking),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert!(!engine.call_order.borrow().contains(&"rejection"));
    }

    #[test]
    fn rekey_stuck_rebuilds_once_per_child_sa() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 290), &mut tracking),
            REKEY_STUCK
        );
        // The terminate did not take; the same CHILD_SA is still listed.
        for expires in [260, 230, 30] {
            assert_eq!(
                rekey_tick(&engine, lifetime(12, expires), &mut tracking),
                SteadyOutcome::StillUp
            );
        }
        assert_eq!(*engine.terminate_calls.borrow(), 1);
    }

    #[test]
    fn rekey_stuck_new_child_sa_rearms() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 290), &mut tracking),
            REKEY_STUCK
        );
        assert_eq!(
            rekey_tick(&engine, lifetime(13, 3960), &mut tracking),
            SteadyOutcome::StillUp
        );
        assert_eq!(
            tracking.rekey_stuck_rebuilt, None,
            "a new CHILD_SA clears the guard"
        );
        assert_eq!(
            rekey_tick(&engine, lifetime(13, 290), &mut tracking),
            REKEY_STUCK
        );
        assert_eq!(*engine.terminate_calls.borrow(), 2);
    }

    #[test]
    fn rekey_stuck_natural_rekey_overlap_never_rebuilds() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        for health in [
            lifetime(12, 700),
            lifetime(12, 400),
            lifetime(12, 370),
            // The rekey landed: the replacement is the newest.
            lifetime(13, 3960),
            lifetime(13, 3930),
        ] {
            assert_eq!(
                rekey_tick(&engine, health, &mut tracking),
                SteadyOutcome::StillUp
            );
        }
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn rekey_stuck_structural_failure_takes_precedence() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 290), &mut tracking),
            REKEY_STUCK
        );
        assert_eq!(
            rekey_tick(&engine, SteadyStateHealth::ChildSaMissing, &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(tracking.rekey_stuck_rebuilt, None);
    }

    #[test]
    fn rekey_stuck_no_lifetime_logs_once_and_never_rebuilds() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        let no_expiry = SteadyStateHealth::Ok(Some(ChildSaLifetime {
            unique_id: 12,
            expires_in: None,
        }));
        for _ in 0..2 {
            assert_eq!(
                rekey_tick(&engine, no_expiry, &mut tracking),
                SteadyOutcome::StillUp
            );
            assert_eq!(tracking.lifetime_missing_logged, Some(12));
        }
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn rekey_stuck_swu_none_does_nothing() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, SteadyStateHealth::Ok(None), &mut tracking),
            SteadyOutcome::StillUp
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    #[test]
    fn rekey_stuck_process_died_clears_guards() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 290), &mut tracking),
            REKEY_STUCK
        );
        *engine.alive.borrow_mut() = false;
        rekey_tick(&engine, lifetime(12, 260), &mut tracking);
        assert_eq!(tracking.rekey_stuck_rebuilt, None);
    }

    #[test]
    fn rekey_stuck_names_the_last_rejection() {
        // The reason is only read once the rebuild is decided; it never
        // triggers one by itself (clarification Q2).
        let engine = FakeEngine {
            rejection: RefCell::new(Some("INVAL_SEL".into())),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 3000), &mut tracking),
            SteadyOutcome::StillUp
        );
        assert_eq!(
            rekey_tick(&engine, lifetime(12, 290), &mut tracking),
            REKEY_STUCK
        );
    }

    /// SC-002: six hours of healthy hourly rekeys at the 30s cadence never
    /// trigger a rebuild, wherever in its jitter window each rekey lands.
    #[test]
    fn rekey_stuck_six_hour_soak_of_natural_rekeys() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        let mut id = 1;
        let mut expires: i64 = 3960;
        // Remaining lifetime at which each successive rekey lands: the edges
        // of the 360-720s window and points between.
        let rekey_at = [360, 720, 400, 661, 365, 500, 710];
        let mut rekeys = 0;
        for _ in 0..720 {
            assert_eq!(
                rekey_tick(&engine, lifetime(id, expires), &mut tracking),
                SteadyOutcome::StillUp,
                "#{id} at {expires}s"
            );
            expires -= 30;
            if expires <= rekey_at[rekeys % rekey_at.len()] {
                id += 1;
                rekeys += 1;
                expires = 3960;
            }
        }
        assert!(rekeys >= 6, "{rekeys} rekeys");
        assert_eq!(*engine.terminate_calls.borrow(), 0);
    }

    /// SC-001/SC-003: a #107-style window -- the same CHILD_SA aging all the
    /// way to its hard expiry -- gets exactly one rebuild, with time to spare.
    #[test]
    fn rekey_stuck_107_replay_rebuilds_once_with_margin() {
        let engine = FakeEngine::default();
        let mut tracking = SteadyTracking::default();
        let mut rebuilt_at = Vec::new();
        let mut expires: i64 = 3960 - 7;
        while expires > 0 {
            if rekey_tick(&engine, lifetime(12, expires), &mut tracking) == REKEY_STUCK {
                rebuilt_at.push(expires);
            }
            expires -= 30;
        }
        assert_eq!(rebuilt_at.len(), 1, "{rebuilt_at:?}");
        assert!(rebuilt_at[0] >= 270, "{rebuilt_at:?}");
    }

    // ---- specs/100 US3: a half-open IKE_SA is charon's to finish -----------

    /// The fingerprint a half-open engine reports, until a test changes it.
    fn busy() -> Option<IkeActivity> {
        Some(IkeActivity {
            lines: 5,
            last: "retransmit 2 of request with message ID 0".to_string(),
        })
    }

    fn half_open_engine(ike_id: u32) -> FakeEngine {
        FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaConnecting { ike_id }),
            activity: RefCell::new(busy()),
            ..Default::default()
        }
    }

    /// A tracking that has been watching `half_open_engine(ike_id)` and seen
    /// nothing new for `idle_secs`.
    fn idle_for(ike_id: u32, idle_secs: u64) -> SteadyTracking {
        SteadyTracking {
            half_open: Some(HalfOpen {
                ike_id,
                activity: busy(),
                active_at: SystemTime::now() - Duration::from_secs(idle_secs),
                waiting_logged: true,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_half_open_ike_sa_with_recent_activity_is_left_alone() {
        let runner = MockCommandRunner::new();
        let engine = half_open_engine(7);
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::WaitingForNegotiation
        );
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(*engine.restart_calls.borrow(), 0);
        let half_open = tracking.half_open.expect("recorded");
        assert_eq!(half_open.ike_id, 7);
        assert!(half_open.waiting_logged);
    }

    #[test]
    fn waiting_on_the_same_ike_sa_is_logged_once_and_never_strikes() {
        let runner = MockCommandRunner::new();
        let engine = half_open_engine(7);
        let mut tracking = SteadyTracking::default();
        for _ in 0..5 {
            assert_eq!(
                tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
                SteadyOutcome::WaitingForNegotiation
            );
        }
        // Five ticks is well past the two strikes a missing CHILD_SA gets.
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.reinitiate_calls.borrow(), 0);
        assert_eq!(tracking.child_sa_missing.count, 0);
        let active_at = tracking.half_open.clone().unwrap().active_at;
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert_eq!(
            tracking.half_open.unwrap().active_at,
            active_at,
            "nothing new from charon, so the idle clock is not reset"
        );
    }

    #[test]
    fn a_half_open_ike_sa_silent_for_over_200s_is_terminated_and_reinitiated() {
        let runner = MockCommandRunner::new();
        let engine = half_open_engine(7);
        let mut tracking = idle_for(7, 201);
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaNegotiationStuck,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.terminate_calls.borrow(), 1);
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(
            *engine.call_order.borrow(),
            vec!["terminate", "reinitiate"],
            "terminate before reinitiate"
        );
        assert!(tracking.half_open.is_none());
        assert!(recovery_restarts_agent(
            DegradeReason::IkeSaNegotiationStuck
        ));
        assert!(recovery_renegotiates(DegradeReason::IkeSaNegotiationStuck));
    }

    #[test]
    fn a_new_ike_sa_number_restarts_the_waiting_record() {
        let runner = MockCommandRunner::new();
        let mut tracking = SteadyTracking::default();
        let first = half_open_engine(7);
        tick_steady_state(&first, &runner, "10.0.0.1", &mut tracking);
        let second = half_open_engine(8);
        assert_eq!(
            tick_steady_state(&second, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::WaitingForNegotiation
        );
        let half_open = tracking.half_open.unwrap();
        assert_eq!(half_open.ike_id, 8);
        assert!(half_open.waiting_logged, "logged again for the new #N");
    }

    #[test]
    fn a_missing_child_sa_under_an_established_ike_sa_keeps_its_two_strikes() {
        // 086 / #107: the half-open handling must not weaken the case it was
        // split from.
        let runner = MockCommandRunner::new();
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ike_header: RefCell::new(Some((3, IkeHeaderState::Established))),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::ChildSaStuck,
                ike_sa_loss_cause: None,
            }
        );
    }

    #[test]
    fn a_half_open_ike_sa_with_no_activity_line_is_timed_from_when_it_was_first_seen() {
        let runner = MockCommandRunner::new();
        let engine = half_open_engine(7);
        *engine.activity.borrow_mut() = None;
        // Seen for the first time just now: not stuck.
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::WaitingForNegotiation
        );
        // Seen 201s ago and silent since: stuck.
        tracking.half_open = Some(HalfOpen {
            ike_id: 7,
            activity: None,
            active_at: SystemTime::now() - Duration::from_secs(201),
            waiting_logged: true,
        });
        assert!(matches!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaNegotiationStuck,
                ..
            }
        ));
    }

    #[test]
    fn new_ike_activity_resets_the_idle_clock_however_old_the_ike_sa() {
        let runner = MockCommandRunner::new();
        let engine = half_open_engine(7);
        // Idle for 250s on the old fingerprint...
        let mut tracking = idle_for(7, 250);
        // ...but charon has logged something since the last look.
        *engine.activity.borrow_mut() = Some(IkeActivity {
            lines: 6,
            last: "retransmit 3 of request with message ID 0".to_string(),
        });
        assert_eq!(
            tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking),
            SteadyOutcome::WaitingForNegotiation
        );
        nothing_was_issued(&engine);
        let idle = SystemTime::now()
            .duration_since(tracking.half_open.unwrap().active_at)
            .unwrap();
        assert!(idle < Duration::from_secs(5), "{idle:?}");
    }

    #[test]
    fn leaving_the_half_open_state_forgets_it() {
        let runner = MockCommandRunner::new();
        let engine = half_open_engine(7);
        let mut tracking = SteadyTracking::default();
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
        tick_steady_state(&engine, &runner, "10.0.0.1", &mut tracking);
        assert!(tracking.half_open.is_none());
    }

    // ---- specs/100: the underlay guard ----------------------------------

    fn view(verdict: UnderlayVerdict) -> UnderlayView {
        UnderlayView {
            enabled: true,
            verdict,
            route_ok: true,
            pending: None,
            heard_recently: false,
            restored: false,
        }
    }

    const NO_ROUTE: UplinkHoldCause = UplinkHoldCause::NoRoute;
    const NO_RESPONSE: UplinkHoldCause = UplinkHoldCause::NoResponse;

    fn held(cause: UplinkHoldCause, skipped: DegradeReason) -> SteadyOutcome {
        SteadyOutcome::Held {
            cause,
            skipped,
            initiated: false,
            ike_sa_loss_cause: None,
        }
    }

    /// One strongswan-cadence tick under `underlay`, for a line selecting
    /// 10.0.0.11 whose agent reports `agent`.
    fn guarded(
        engine: &FakeEngine,
        tracking: &mut SteadyTracking,
        underlay: &UnderlayView,
        agent: Option<&PoolState>,
    ) -> SteadyOutcome {
        let current_pool = pool(&["10.0.0.11"]);
        super::tick_steady_state_underlay(
            engine,
            &MockCommandRunner::new(),
            "10.0.0.11",
            &current_pool,
            tracking,
            STEADY_STATE_POLL_INTERVAL,
            &|| false,
            &|| agent.cloned(),
            underlay,
        )
    }

    fn nothing_was_issued(engine: &FakeEngine) {
        assert_eq!(*engine.terminate_calls.borrow(), 0, "terminate");
        assert_eq!(*engine.reinitiate_calls.borrow(), 0, "reinitiate");
        assert_eq!(*engine.restart_calls.borrow(), 0, "restart");
        assert_eq!(*engine.recreate_interface_calls.borrow(), 0, "recreate");
    }

    fn exhausted_agent() -> PoolState {
        state(vec![agent_mark(
            "10.0.0.11",
            "gm-keepalive",
            at(BASELINE + 5),
        )])
    }

    #[test]
    fn every_degrade_reason_is_classified_as_inferred_or_structural() {
        // No wildcard: a new reason must be classified here too.
        for reason in [
            DegradeReason::ProcessDied,
            DegradeReason::ViciBroken,
            DegradeReason::TunVanished,
            DegradeReason::TunUnavailable,
            DegradeReason::ChildSaMissing,
            DegradeReason::IkeSaLost,
            DegradeReason::ChildSaStuck,
            DegradeReason::ChildSaRekeyStuck,
            DegradeReason::PcscfChanged,
            DegradeReason::PcscfUnreachable,
            DegradeReason::DefaultRouteMissing,
            DegradeReason::PcscfPoolExhausted,
            DegradeReason::IkeSaNegotiationStuck,
        ] {
            let expected = match reason {
                DegradeReason::PcscfUnreachable
                | DegradeReason::PcscfPoolExhausted
                | DegradeReason::IkeSaLost
                | DegradeReason::ChildSaMissing
                | DegradeReason::ChildSaStuck
                | DegradeReason::ChildSaRekeyStuck
                | DegradeReason::IkeSaNegotiationStuck => true,
                DegradeReason::ProcessDied
                | DegradeReason::ViciBroken
                | DegradeReason::TunVanished
                | DegradeReason::TunUnavailable
                | DegradeReason::DefaultRouteMissing
                | DegradeReason::PcscfChanged => false,
            };
            assert_eq!(is_reachability_inferred(reason), expected, "{reason:?}");
        }
    }

    #[test]
    fn the_disabled_view_never_holds_or_defers() {
        let v = UnderlayView::disabled();
        assert!(!v.enabled);
        assert_eq!(v.verdict, UnderlayVerdict::Up);
        assert_eq!(v.holding(), None);
        // Even a failed route check says nothing when the guard is off.
        let off = UnderlayView {
            route_ok: false,
            verdict: UnderlayVerdict::NoRoute,
            ..v
        };
        assert_eq!(off.holding(), None);
    }

    #[test]
    fn a_dead_pcscf_path_is_held_not_rebuilt_while_the_uplink_is_down() {
        for (verdict, cause) in [
            (UnderlayVerdict::NoRoute, NO_ROUTE),
            (UnderlayVerdict::NoResponse, NO_RESPONSE),
        ] {
            let engine = FakeEngine {
                pcscf_reachable: RefCell::new(false),
                ..Default::default()
            };
            let mut tracking = SteadyTracking::default();
            let strikes = pcscf_unreachable_strikes(STEADY_STATE_POLL_INTERVAL);
            let mut last = SteadyOutcome::StillUp;
            for _ in 0..strikes {
                last = guarded(&engine, &mut tracking, &view(verdict), None);
            }
            assert_eq!(last, held(cause, DegradeReason::PcscfUnreachable));
            nothing_was_issued(&engine);
            assert_eq!(
                tracking.pcscf_unreachable.count, 0,
                "strikes against a dead path are not evidence once it is back"
            );
        }
    }

    #[test]
    fn an_exhausted_pool_is_held_while_the_uplink_is_down() {
        for (verdict, cause) in [
            (UnderlayVerdict::NoRoute, NO_ROUTE),
            (UnderlayVerdict::NoResponse, NO_RESPONSE),
        ] {
            let engine = FakeEngine::default();
            let mut tracking = negotiated();
            let outcome = guarded(
                &engine,
                &mut tracking,
                &view(verdict),
                Some(&exhausted_agent()),
            );
            assert_eq!(outcome, held(cause, DegradeReason::PcscfPoolExhausted));
            nothing_was_issued(&engine);
            assert!(tracking.pool_exhausted.is_empty());
        }
    }

    #[test]
    fn a_missing_child_sa_and_a_stuck_one_are_held_while_the_uplink_is_down() {
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::ChildSaMissing),
            ike_header: RefCell::new(Some((3, IkeHeaderState::Established))),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::NoRoute),
                None
            ),
            held(NO_ROUTE, DegradeReason::ChildSaMissing)
        );
        // The strike was not earned against a working path.
        assert_eq!(tracking.child_sa_missing.count, 0);
        tracking.child_sa_missing = Streak {
            count: 1,
            covered: STEADY_STATE_POLL_INTERVAL,
        };
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::NoResponse),
                None
            ),
            held(NO_RESPONSE, DegradeReason::ChildSaStuck)
        );
        nothing_was_issued(&engine);
    }

    #[test]
    fn a_stuck_rekey_is_held_while_the_uplink_is_down() {
        let engine = FakeEngine {
            health: RefCell::new(lifetime(4, 100)),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::NoRoute),
                None
            ),
            held(NO_ROUTE, DegradeReason::ChildSaRekeyStuck)
        );
        nothing_was_issued(&engine);
        assert_eq!(
            tracking.rekey_stuck_rebuilt, None,
            "held, so the one rebuild it is owed is still to come"
        );
    }

    #[test]
    fn a_held_stuck_rekey_still_lets_a_missing_route_be_repaired() {
        let engine = FakeEngine {
            health: RefCell::new(lifetime(4, 100)),
            route_missing: RefCell::new(true),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::NoRoute),
                None
            ),
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.repair_calls.borrow(), 1);
        nothing_was_issued(&engine);
        assert_eq!(tracking.rekey_stuck_rebuilt, None);
    }

    #[test]
    fn a_stuck_half_open_ike_sa_is_held_while_the_uplink_is_down() {
        let engine = half_open_engine(7);
        let mut tracking = idle_for(7, 201);
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::NoResponse),
                None
            ),
            held(NO_RESPONSE, DegradeReason::IkeSaNegotiationStuck)
        );
        nothing_was_issued(&engine);
        assert!(tracking.half_open.is_some(), "still the same IKE_SA");
    }

    #[test]
    fn a_lost_ike_sa_gets_exactly_one_initiation_per_hold() {
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            loss_cause: RefCell::new(IkeSaLossCause::DeadPeer),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        let first = guarded(
            &engine,
            &mut tracking,
            &view(UnderlayVerdict::NoResponse),
            None,
        );
        assert_eq!(
            first,
            SteadyOutcome::Held {
                cause: NO_RESPONSE,
                skipped: DegradeReason::IkeSaLost,
                initiated: true,
                ike_sa_loss_cause: Some(IkeSaLossCause::DeadPeer),
            }
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(*engine.terminate_calls.borrow(), 0);
        assert_eq!(*engine.restart_calls.borrow(), 0);
        // A dead peer seen on a dead path is the uplink's doing (FR-022).
        assert_eq!(
            tracking.dead_peer.map(|c| c.verdict),
            Some(DeadPeerVerdict::Uplink)
        );
        // Later ticks with still no IKE_SA issue nothing more.
        for _ in 0..3 {
            let later = guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::NoResponse),
                None,
            );
            assert_eq!(
                later,
                SteadyOutcome::Held {
                    cause: NO_RESPONSE,
                    skipped: DegradeReason::IkeSaLost,
                    initiated: false,
                    ike_sa_loss_cause: None,
                }
            );
        }
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
    }

    #[test]
    fn the_held_initiation_is_owed_again_once_an_ike_sa_has_been_listed() {
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            ..Default::default()
        };
        let mut tracking = SteadyTracking::default();
        let down = view(UnderlayVerdict::NoRoute);
        guarded(&engine, &mut tracking, &down, None);
        // charon re-established, then lost it again, still on a dead path.
        *engine.health.borrow_mut() = SteadyStateHealth::Ok(None);
        guarded(&engine, &mut tracking, &down, None);
        *engine.health.borrow_mut() = SteadyStateHealth::IkeSaMissing;
        guarded(&engine, &mut tracking, &down, None);
        assert_eq!(*engine.reinitiate_calls.borrow(), 2);
    }

    #[test]
    fn a_half_open_ike_sa_counts_as_one_listed_so_no_initiation_is_owed() {
        // FR-010a: never initiate while any IKE_SA exists, half-open included.
        let engine = half_open_engine(7);
        let mut tracking = SteadyTracking::default();
        guarded(
            &engine,
            &mut tracking,
            &view(UnderlayVerdict::NoRoute),
            None,
        );
        nothing_was_issued(&engine);
    }

    #[test]
    fn structural_recoveries_run_whatever_the_uplink_verdict() {
        for verdict in [UnderlayVerdict::NoRoute, UnderlayVerdict::NoResponse] {
            // Process died.
            let engine = FakeEngine {
                alive: RefCell::new(false),
                ..Default::default()
            };
            assert_eq!(
                guarded(
                    &engine,
                    &mut SteadyTracking::default(),
                    &view(verdict),
                    None
                ),
                SteadyOutcome::Recovered {
                    reason: DegradeReason::ProcessDied,
                    ike_sa_loss_cause: None
                }
            );
            // vici broken.
            let engine = FakeEngine {
                health: RefCell::new(SteadyStateHealth::ViciBroken),
                ..Default::default()
            };
            assert!(matches!(
                guarded(
                    &engine,
                    &mut SteadyTracking::default(),
                    &view(verdict),
                    None
                ),
                SteadyOutcome::Recovered {
                    reason: DegradeReason::ViciBroken,
                    ..
                }
            ));
            // Interface vanished.
            let engine = FakeEngine {
                health: RefCell::new(SteadyStateHealth::TunVanished),
                ..Default::default()
            };
            assert!(matches!(
                guarded(
                    &engine,
                    &mut SteadyTracking::default(),
                    &view(verdict),
                    None
                ),
                SteadyOutcome::Recovered {
                    reason: DegradeReason::TunVanished,
                    ..
                }
            ));
            // The namespace's default route is gone: repaired on the same tick.
            let engine = FakeEngine {
                route_missing: RefCell::new(true),
                ..Default::default()
            };
            assert!(matches!(
                guarded(
                    &engine,
                    &mut SteadyTracking::default(),
                    &view(verdict),
                    None
                ),
                SteadyOutcome::Recovered {
                    reason: DegradeReason::DefaultRouteMissing,
                    ..
                }
            ));
        }
    }

    #[test]
    fn with_the_uplink_up_every_recovery_is_taken_as_before() {
        let up = view(UnderlayVerdict::Up);
        // Pool exhausted, recent life: no liveness check needed.
        let recent = UnderlayView {
            heard_recently: true,
            ..up
        };
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        assert_eq!(
            guarded(&engine, &mut tracking, &recent, Some(&exhausted_agent())),
            POOL_EXHAUSTED
        );
        assert_eq!(*engine.dpd_calls.borrow(), 0);
        // A lost IKE_SA is re-initiated, the agent restarted.
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            ..Default::default()
        };
        assert!(matches!(
            guarded(&engine, &mut SteadyTracking::default(), &up, None),
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaLost,
                ..
            }
        ));
    }

    // Data-model row 7: one failed route check, with a recovery due.
    #[test]
    fn one_failed_route_check_holds_a_recovery_that_is_due() {
        let failed_once = UnderlayView {
            route_ok: false,
            ..view(UnderlayVerdict::Up)
        };
        let engine = FakeEngine::default();
        let mut tracking = negotiated();
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &failed_once,
                Some(&exhausted_agent())
            ),
            held(NO_ROUTE, DegradeReason::PcscfPoolExhausted)
        );
        nothing_was_issued(&engine);
        assert_eq!(*engine.dpd_calls.borrow(), 0, "no DPD without a route");
    }

    #[test]
    fn one_failed_route_check_with_nothing_due_is_just_a_quiet_tick() {
        let failed_once = UnderlayView {
            route_ok: false,
            ..view(UnderlayVerdict::Up)
        };
        let engine = FakeEngine::default();
        assert_eq!(
            guarded(&engine, &mut SteadyTracking::default(), &failed_once, None),
            SteadyOutcome::StillUp
        );
    }

    // Data-model row 8: nothing proves the ePDG alive.
    fn established_engine() -> FakeEngine {
        FakeEngine {
            ike_header: RefCell::new(Some((3, IkeHeaderState::Established))),
            ..Default::default()
        }
    }

    #[test]
    fn an_inferred_recovery_with_no_sign_of_life_sends_one_dpd_and_waits() {
        let engine = established_engine();
        let mut tracking = negotiated();
        let outcome = guarded(
            &engine,
            &mut tracking,
            &view(UnderlayVerdict::Up),
            Some(&exhausted_agent()),
        );
        assert_eq!(
            outcome,
            SteadyOutcome::Deferred {
                reason: DegradeReason::PcscfPoolExhausted
            }
        );
        assert_eq!(*engine.dpd_calls.borrow(), 1);
        nothing_was_issued(&engine);
    }

    #[test]
    fn recent_ike_life_or_recent_esp_means_the_recovery_is_genuine() {
        // The ePDG spoke since the last tick.
        let engine = established_engine();
        let spoke = UnderlayView {
            heard_recently: true,
            ..view(UnderlayVerdict::Up)
        };
        assert_eq!(
            guarded(&engine, &mut negotiated(), &spoke, Some(&exhausted_agent())),
            POOL_EXHAUSTED
        );
        assert_eq!(*engine.dpd_calls.borrow(), 0);
        // Inbound ESP is younger than 30s.
        let engine = established_engine();
        *engine.inbound.borrow_mut() = esp_seen(5);
        assert_eq!(
            guarded(
                &engine,
                &mut negotiated(),
                &view(UnderlayVerdict::Up),
                Some(&exhausted_agent())
            ),
            POOL_EXHAUSTED
        );
        assert_eq!(*engine.dpd_calls.borrow(), 0);
    }

    #[test]
    fn old_esp_does_not_count_as_life() {
        let engine = established_engine();
        *engine.inbound.borrow_mut() = esp_seen(31);
        assert!(matches!(
            guarded(
                &engine,
                &mut negotiated(),
                &view(UnderlayVerdict::Up),
                Some(&exhausted_agent())
            ),
            SteadyOutcome::Deferred { .. }
        ));
    }

    #[test]
    fn no_dpd_is_sent_without_an_established_ike_sa_to_send_it_on() {
        // No header at all, or a half-open one: proceed as before.
        let engine = FakeEngine::default();
        assert_eq!(
            guarded(
                &engine,
                &mut negotiated(),
                &view(UnderlayVerdict::Up),
                Some(&exhausted_agent())
            ),
            POOL_EXHAUSTED
        );
        assert_eq!(*engine.dpd_calls.borrow(), 0);
    }

    #[test]
    fn a_lost_ike_sa_is_never_deferred_for_a_dpd() {
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            ike_header: RefCell::new(Some((3, IkeHeaderState::Established))),
            ..Default::default()
        };
        assert!(matches!(
            guarded(
                &engine,
                &mut SteadyTracking::default(),
                &view(UnderlayVerdict::Up),
                None
            ),
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaLost,
                ..
            }
        ));
        assert_eq!(*engine.dpd_calls.borrow(), 0);
    }

    // Data-model row 6: the check has had its 30s.
    fn with_pending(life: bool) -> UnderlayView {
        UnderlayView {
            pending: Some(PendingView {
                reason: DegradeReason::PcscfPoolExhausted,
                life_since_mark: life,
                expired: true,
            }),
            ..view(UnderlayVerdict::Up)
        }
    }

    #[test]
    fn a_check_still_inside_its_wait_keeps_the_recovery_deferred_without_a_second_one() {
        // The 5s ticks around a hold come back long before the 30s an answer
        // is given: silence by then is no answer either way.
        let engine = established_engine();
        let waiting = UnderlayView {
            pending: Some(PendingView {
                reason: DegradeReason::PcscfPoolExhausted,
                life_since_mark: false,
                expired: false,
            }),
            ..view(UnderlayVerdict::Up)
        };
        assert_eq!(
            guarded(
                &engine,
                &mut negotiated(),
                &waiting,
                Some(&exhausted_agent())
            ),
            SteadyOutcome::Deferred {
                reason: DegradeReason::PcscfPoolExhausted
            }
        );
        nothing_was_issued(&engine);
        assert_eq!(*engine.dpd_calls.borrow(), 0, "no second check");
    }

    #[test]
    fn an_answered_check_lets_the_recovery_through() {
        let engine = established_engine();
        assert_eq!(
            guarded(
                &engine,
                &mut negotiated(),
                &with_pending(true),
                Some(&exhausted_agent())
            ),
            POOL_EXHAUSTED
        );
        assert_eq!(*engine.dpd_calls.borrow(), 0, "no second check");
    }

    #[test]
    fn an_unanswered_check_holds_the_line_as_no_response() {
        let engine = established_engine();
        assert_eq!(
            guarded(
                &engine,
                &mut negotiated(),
                &with_pending(false),
                Some(&exhausted_agent())
            ),
            held(NO_RESPONSE, DegradeReason::PcscfPoolExhausted)
        );
        nothing_was_issued(&engine);
        assert_eq!(*engine.dpd_calls.borrow(), 0, "no second check");
    }

    #[test]
    fn a_dpd_that_could_not_be_queued_still_defers() {
        let engine = established_engine();
        *engine.dpd_accepted.borrow_mut() = false;
        assert!(matches!(
            guarded(
                &engine,
                &mut negotiated(),
                &view(UnderlayVerdict::Up),
                Some(&exhausted_agent())
            ),
            SteadyOutcome::Deferred { .. }
        ));
        assert_eq!(*engine.dpd_calls.borrow(), 1);
    }

    // The US2 regressions: a real carrier fault is still repaired.
    #[test]
    fn a_single_pcscf_black_hole_with_a_live_epdg_is_rebuilt_after_one_dpd() {
        let engine = established_engine();
        let mut tracking = negotiated();
        let agent = exhausted_agent();
        // Tick 1: nothing proves the ePDG alive, so ask it.
        assert!(matches!(
            guarded(
                &engine,
                &mut tracking,
                &view(UnderlayVerdict::Up),
                Some(&agent)
            ),
            SteadyOutcome::Deferred { .. }
        ));
        // Tick 2: it answered, so the rebuild goes ahead, never `Held`.
        assert_eq!(
            guarded(&engine, &mut tracking, &with_pending(true), Some(&agent)),
            POOL_EXHAUSTED
        );
    }

    #[test]
    fn a_missing_namespace_route_is_repaired_on_the_same_tick_whatever_the_verdict() {
        let engine = FakeEngine {
            route_missing: RefCell::new(true),
            pcscf_reachable: RefCell::new(false),
            ..Default::default()
        };
        assert!(matches!(
            guarded(
                &engine,
                &mut SteadyTracking::default(),
                &view(UnderlayVerdict::NoResponse),
                None
            ),
            SteadyOutcome::Recovered {
                reason: DegradeReason::DefaultRouteMissing,
                ..
            }
        ));
        assert_eq!(*engine.route_repairs.borrow(), 1);
    }

    #[test]
    fn a_carrier_delete_of_the_ike_sa_is_reinitiated_as_today_when_the_uplink_is_up() {
        let engine = FakeEngine {
            health: RefCell::new(SteadyStateHealth::IkeSaMissing),
            loss_cause: RefCell::new(IkeSaLossCause::CarrierDelete),
            ..Default::default()
        };
        assert_eq!(
            guarded(
                &engine,
                &mut SteadyTracking::default(),
                &view(UnderlayVerdict::Up),
                None
            ),
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaLost,
                ike_sa_loss_cause: Some(IkeSaLossCause::CarrierDelete),
            }
        );
        assert_eq!(*engine.reinitiate_calls.borrow(), 1);
        assert_eq!(*engine.restart_calls.borrow(), 0);
    }

    #[test]
    fn the_guard_off_behaves_exactly_as_before_for_a_pool_exhaustion() {
        let engine = established_engine();
        let mut tracking = negotiated();
        assert_eq!(
            guarded(
                &engine,
                &mut tracking,
                &UnderlayView::disabled(),
                Some(&exhausted_agent())
            ),
            POOL_EXHAUSTED
        );
        assert_eq!(*engine.dpd_calls.borrow(), 0);
    }

    #[test]
    fn a_half_open_ike_sa_is_retried_at_once_when_a_no_route_hold_ends() {
        let engine = half_open_engine(7);
        let restored = UnderlayView {
            restored: true,
            ..view(UnderlayVerdict::Up)
        };
        let mut tracking = SteadyTracking::default();
        assert_eq!(
            guarded(&engine, &mut tracking, &restored, None),
            SteadyOutcome::Recovered {
                reason: DegradeReason::IkeSaNegotiationStuck,
                ike_sa_loss_cause: None,
            }
        );
        assert_eq!(*engine.call_order.borrow(), vec!["terminate", "reinitiate"]);
        assert!(tracking.half_open.is_none());
        // The same half-open IKE_SA on an ordinary tick is left alone.
        let engine = half_open_engine(7);
        assert_eq!(
            guarded(
                &engine,
                &mut SteadyTracking::default(),
                &view(UnderlayVerdict::Up),
                None
            ),
            SteadyOutcome::WaitingForNegotiation
        );
        nothing_was_issued(&engine);
        // And with the guard off the flag means nothing.
        let off = UnderlayView {
            restored: true,
            ..UnderlayView::disabled()
        };
        let engine = half_open_engine(7);
        assert_eq!(
            guarded(&engine, &mut SteadyTracking::default(), &off, None),
            SteadyOutcome::WaitingForNegotiation
        );
    }
}
