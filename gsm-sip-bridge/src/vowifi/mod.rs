//! Agent B: the SIP/PBX-facing half of the inbound VoWiFi bridge (see
//! `specs/011-vowifi-sip-bridge/`). Runs in the container's default network
//! namespace (LAN-reachable to the PBX), receives `IncomingCall` events from
//! Agent A (`crate::ims::agent`, running in the tunnel's `ims` netns) over
//! the control channel defined in `control`, and bridges each call by
//! placing two PJSIP calls — one to the configured PBX destination, one back
//! to Agent A across the veth link — and conference-connecting them
//! (`pjsua_safe::Endpoint::pair_calls`, `specs/011-vowifi-sip-bridge`
//! Foundational T010).
//!
//! Deliberately builds its own `Endpoint`/`Account` here rather than reusing
//! `crate::sip::SipBridge`: `SipBridge` holds a single `active_call:
//! Option<Call>` (correct for the circuit-switched bridge, which only ever
//! has one call at a time) and has no accessor for its private `Endpoint` —
//! this feature needs to hold *two* concurrent `Call`s and pair them, which
//! doesn't fit that shape. Building a second `Endpoint`/`Account` here is a
//! few duplicated lines, not a new abstraction, and leaves `SipBridge`/the
//! existing circuit-switched call path completely untouched (FR-006).

pub mod control;
pub mod discovery;
pub mod ims_mode;
pub mod imsi;
pub mod plmn;
pub mod usim_bridge;

use crate::config::{AppConfig, SipTransport as ConfigSipTransport, TlsVerify, VowifiConfig};
use crate::control::protocol::{
    AgentKind, AgentState, ObservedEvent, OutboundAttemptOutcome, SmsOutcome,
};
use crate::error::{BridgeError, BridgeResult};
use crate::modules::discovery::lines_file_path;
use crate::observability::reporter::Reporter;
use crate::sms;
use crate::sms::discord::DiscordClient;
use crate::store::{StoreCommand, StoreHandle};
use control::{read_msg, write_msg, CallRecord, ControlMessage};
use pjsua_safe::{
    Account, AccountConfig, Call, CallState, Endpoint, EndpointConfig, TransportType,
};
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::ExitCode;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::runtime::Runtime;

/// Fixed port Agent A listens on for Agent B's inbound (veth-internal) SIP
/// call. Not user-configurable — this is a private implementation detail of
/// the link between the two agents, not something an operator ever points
/// at directly (unlike `[vowifi].control_port`, which is documented config
/// because it's part of the deployment's env/compose wiring).
pub const VETH_SIP_PORT: u16 = 5070;
/// Fixed port Agent A listens on for `vowifi-status` registration-health
/// queries (`ControlMessage::StatusQuery` → `RegistrationStatusReply`).
/// Same "private implementation detail" status as `VETH_SIP_PORT`.
pub const AGENT_A_STATUS_PORT: u16 = 5071;
/// Agent B's own local SIP port for its PJSIP endpoint — deliberately NOT
/// `[sip].local_port`. Both the circuit-switched daemon and Agent B share
/// one config file and, in the merged deployment (`supervise::orchestrate`),
/// one network namespace (host networking) — reusing `[sip].local_port` for
/// both means two independent `pjsua_create`/transport-bind calls racing for
/// the same UDP port, which fails outright for whichever one starts second.
/// Same "private implementation detail" status as `VETH_SIP_PORT`/
/// `AGENT_A_STATUS_PORT`.
pub const AGENT_B_SIP_LOCAL_PORT: u16 = 5072;

/// How long to wait for the PBX to give a *final* response to the initial
/// REGISTER before treating it as not-yet-confirmed and backing off. A SIP
/// registration, even with an auth round-trip, settles in a second or two; this
/// leaves generous headroom.
const PBX_REG_CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);
/// First delay after an unconfirmed/denied REGISTER before re-checking.
const PBX_REG_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Ceiling on the re-check backoff. Deliberately unhurried: PJSUA re-registers
/// on its own timer and hammering a denial is what triggers auth lockouts in
/// the first place, so we poll its live status rather than force REGISTERs.
const PBX_REG_MAX_BACKOFF: Duration = Duration::from_secs(300);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bounded history of recent call outcomes (FR-008, User Story 3) — oldest
/// evicted once full so memory stays flat over an arbitrarily long uptime.
/// `capacity` is fixed at construction; not user-configurable, since this
/// is an operational diagnostic aid, not a feature an operator tunes.
pub struct RecentCalls {
    capacity: usize,
    records: VecDeque<CallRecord>,
}

impl RecentCalls {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            records: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, record: CallRecord) {
        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Newest first — the order an operator checking status wants to see.
    pub fn snapshot(&self) -> Vec<CallRecord> {
        self.records.iter().rev().cloned().collect()
    }
}

/// How many recent call outcomes to remember for `vowifi-status`.
const RECENT_CALLS_CAPACITY: usize = 20;

/// One VoWiFi line as far as Agent B and `vowifi-status` care: just enough
/// to open a control-channel listener (Agent B) or query one (`vowifi-
/// status`) — `card_id`, Agent A's veth-local address (status port, same
/// netns Agent A runs in but reachable from the default netns over the
/// veth link) and Agent B's own veth-peer address/control port (the
/// control-channel listener this line's Agent A connects to).
/// specs/013-multi-card-vowifi, `contracts/agent-topology-contract.md`.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeLine {
    pub index: u32,
    pub card_id: String,
    pub veth_local_addr: String,
    pub veth_peer_addr: String,
    pub control_port: u16,
    /// SIP port on `veth_local_addr` where the carrier-side half listens for
    /// this half's leg. [`VETH_SIP_PORT`] over a veth for the Wi-Fi path;
    /// `volte::bridge::LOOPBACK_SIP_PORT` over loopback for the cellular one,
    /// which is the only thing that differs between them here.
    pub sip_leg_port: u16,
    /// specs/034-alert-identity: this line's configured `[[vowifi.line]].msisdn`
    /// (resolved from the override that pinned it), shown when forwarding this
    /// line's SMS to Discord. `None` ⇒ the forward's phone field renders
    /// `unknown`.
    pub msisdn: Option<String>,
}

/// Reads the `discover` subcommand's line-resolution file and returns every
/// resolved VoWiFi line. Every deployment — including a single-SIM one —
/// runs `discover` first (`supervise::orchestrate` always does); a missing or
/// empty line-resolution file is a real startup error, not a signal to fall
/// back to raw `[vowifi]` config (there is no longer a raw single-line
/// config to fall back to — see `VowifiConfig`'s per-line-only fields).
fn resolve_runtime_lines(_config: &VowifiConfig) -> BridgeResult<Vec<RuntimeLine>> {
    let path = lines_file_path();
    let resolution = discovery::read_line_resolution(&path).map_err(BridgeError::Config)?;
    if resolution.lines.is_empty() {
        return Err(BridgeError::Config(format!(
            "no VoWiFi lines resolved in {} — run `gsm-sip-bridge discover` first",
            path.display()
        )));
    }
    Ok(runtime_lines_from_resolution(&resolution))
}

fn runtime_lines_from_resolution(resolution: &discovery::LineResolution) -> Vec<RuntimeLine> {
    resolution
        .lines
        .iter()
        .map(|l| RuntimeLine {
            index: l.index,
            card_id: l.card_id.clone(),
            veth_local_addr: l.veth_local_addr.clone(),
            veth_peer_addr: l.veth_peer_addr.clone(),
            control_port: l.control_port,
            sip_leg_port: VETH_SIP_PORT,
            // specs/034-alert-identity: the msisdn of the `[[vowifi.line]]`
            // override that pinned this line (auto-discovered lines have none).
            msisdn: l.configured_msisdn(),
        })
        .collect()
}

/// specs/027-discover-retry-health FR-006/FR-007: `vowifi-status`'s
/// "Configured line ... NOT RUNNING" section is only for a configured
/// override (`modem_port`/`modem_serial`/`pcsc_reader`) that failed to
/// start (see `contracts/vowifi-status-output.md`) — checked via
/// `FailedLine::configured` rather than excluding `max_lines_exceeded` by
/// name (an earlier version of this function did that, on the wrong
/// assumption that `max_lines_exceeded` was the *only* reason an
/// unpinned, auto-discovered candidate's failure could show up here:
/// review on this PR found that every other rejection reason —
/// `sim_unreadable`, `sim_locked`, `no_at_port` — was just as reachable
/// for an auto-discovered modem and got mislabeled as "a configured line
/// from config.toml", sending operators looking for a config entry that
/// does not exist). `resolve_lines` now sets `configured` at the point
/// each `FailedLine` is created, which is the only place true provenance
/// is known; a `max_lines_exceeded` overflow of an *actually pinned*
/// modem still correctly counts here, since it is `configured: true`.
pub(crate) fn is_configured_line_failure(failed: &discovery::FailedLine) -> bool {
    failed.configured
}

pub fn run(config: &AppConfig) -> ExitCode {
    match run_inner(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(config: &AppConfig) -> BridgeResult<()> {
    let lines = resolve_runtime_lines(&config.vowifi)?;
    run_telephony_side(
        config,
        AGENT_B_SIP_LOCAL_PORT,
        config.vowifi.wideband,
        lines,
        "vowifi-sip-agent",
        crate::store::Transport::Vowifi,
        // Wi-Fi Agent A/B are separate processes, so there is no in-process flag
        // to share; Agent A's health does not track the PBX leg here.
        None,
    )
}

/// The telephone-system half: one PJSIP endpoint, one PBX registration, and
/// an accept loop per line waiting for the carrier-side half to signal a call.
///
/// Parameterised rather than duplicated because the host-side cellular service
/// (specs/017-volte-inbound-bridge) needs exactly this, differing only in
/// which local port it binds and which addresses its lines sit on — a copy
/// would be a second implementation of PBX registration, codec priority and
/// call bridging, which FR-019 exists to prevent.
///
/// `local_port` **must** be distinct per caller: two `pjsua_create`/
/// transport-bind calls racing for one UDP port fail outright for whichever
/// starts second (research R3).
pub(crate) fn run_telephony_side(
    config: &AppConfig,
    local_port: u16,
    wideband: bool,
    lines: Vec<RuntimeLine>,
    agent_label: &str,
    // `record_transport` is which transport this line's calls and messages are
    // recorded under — named apart from the PJSIP `transport` below, which is
    // a different thing entirely.
    record_transport: crate::store::Transport,
    // Shared with the carrier-side half so its health/admission can tell whether
    // the PBX leg is actually usable — set once this half confirms the PBX
    // accepted its REGISTER, cleared while it hasn't. `None` on the Wi-Fi path,
    // where the two halves are separate processes and cannot share it.
    pbx_registered: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> BridgeResult<()> {
    let transport = match config.sip.transport {
        ConfigSipTransport::Udp => TransportType::Udp,
        ConfigSipTransport::Tcp => TransportType::Tcp,
        ConfigSipTransport::Tls => TransportType::Tls,
    };
    let ep_config = EndpointConfig {
        transport,
        local_port,
        tls_verify: config.sip.tls_verify == TlsVerify::Strict,
        // Everything crossing PJMEDIA's conference bridge is resampled to this
        // rate, so it is the ceiling on what the PBX leg can carry: at 8000, a
        // carrier's 16 kHz AMR-WB would be squeezed through 8 kHz here even if
        // the PBX had happily agreed to G.722.
        clock_rate: if wideband { 16000 } else { 8000 },
        jb_init_ms: config.audio.settings.jb_init_ms,
        jb_min_pre: config.audio.settings.jb_min_pre,
        jb_max_ms: config.audio.settings.jb_max_ms,
        vad_enabled: config.audio.vad,
        // No physical sound device in this process (null snd dev, below) —
        // tx_level only matters for the slot-0 sound-device path.
        tx_level: 1.0,
        snd_rec_latency_ms: config.audio.snd_rec_latency_ms,
        snd_play_latency_ms: config.audio.snd_play_latency_ms,
    };
    let endpoint = Endpoint::create(ep_config)
        .map_err(|e| BridgeError::Ims(format!("PJSIP endpoint creation failed: {e}")))?;
    endpoint
        .set_null_sound_device()
        .map_err(|e| BridgeError::Ims(format!("null sound device setup failed: {e}")))?;
    // Without this, an unsolicited INVITE to this account queues forever
    // with no response when outbound dialing is off — nothing in this
    // process ever calls `poll_incoming_call` to drain it otherwise
    // (specs/025-outbound-calling review).
    endpoint.set_accept_incoming_calls(config.outbound.enabled);
    if wideband {
        prioritize_wideband_codecs(&endpoint);
    }

    // In SIP server mode this process is the one that would have owned the PBX
    // trunk (see `crate::sip::SipBridge`'s `register_trunk`), so it is the one
    // that hosts the registrar. Reusing that existing arbitration is what lets
    // a single registrar serve all three call paths without IPC or a fourth
    // supervised process (spec 024, research.md R-003).
    //
    // Held for the lifetime of this function: dropping it stops the thread and
    // closes the socket, so phones would silently stop being reachable.
    let mut _registrar = None;
    let bindings = if config.sip_server.enabled {
        let server = &config.sip_server;
        // This process serves no `/metrics`, so gauges set here would never be
        // scraped. Forward them to the daemon over the same reporting channel
        // the VoWiFi gauges already use (spec 024, FR-022) — confirmed missing
        // on a live container before this existed.
        let metrics_reporter = Reporter::spawn(
            config.control.socket_path.clone(),
            AgentKind::Sip,
            // The registrar is one per process, not per line, so it reports
            // under the agent label rather than a card id — matching the
            // unlabelled gauges it feeds.
            "sip-server".to_string(),
            Duration::from_secs(config.metrics.agent_report_interval_seconds),
        );
        let observer: crate::sip::server::RegistrarObserver =
            Box::new(move |bindings, ring_registered| {
                metrics_reporter.report(
                    AgentState {
                        sip_server_bindings: Some(bindings),
                        sip_server_ring_registered: Some(ring_registered),
                        ..Default::default()
                    },
                    Vec::new(),
                );
            });
        let outbound_local_port = config.outbound.enabled.then_some(local_port);
        let registrar = crate::sip::server::Registrar::start_observed(
            server,
            outbound_local_port,
            Some(observer),
        )
        .map_err(|e| {
            BridgeError::Ims(format!(
                "SIP registrar could not listen on {}:{}: {e}",
                server.listen_addr, server.listen_port
            ))
        })?;
        let bindings = registrar.bindings();
        let id_uri = server.identity_uri();
        let account = Account::local(&endpoint, &id_uri, &config.sip.display_name)
            .map_err(|e| BridgeError::Ims(format!("local SIP account creation failed: {e}")))?;
        tracing::info!(
            listen = %format!("{}:{}", server.listen_addr, server.listen_port),
            uac_port = local_port,
            ring_aor = %server.ring_aor,
            agent = agent_label,
            "SIP server mode active — IP phones register here; no PBX is used"
        );
        // The carrier half gates admission on this. There is no PBX to confirm
        // acceptance with, and the registrar is listening, so the outbound leg
        // is as available as it will ever be — whether a *phone* is registered
        // is decided per call, when the destination is resolved.
        if let Some(flag) = &pbx_registered {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        _registrar = Some(registrar);
        (Some(bindings), account)
    } else {
        let acc_config = AccountConfig {
            sip_server: config.sip.server.clone(),
            sip_port: config.sip.port,
            username: config.sip.username.clone(),
            password: config.sip.password.expose_secret().clone(),
            display_name: config.sip.display_name.clone(),
        };
        let account = Account::register(&endpoint, acc_config, None)
            .map_err(|e| BridgeError::Ims(format!("SIP account registration failed: {e}")))?;
        (None, account)
    };
    let (bindings, account) = bindings;

    // Trunk mode's analog of `bindings`' source-of-truth for "who may reach
    // the dial-out account" — see `sip::SipBridge::trunk_source_ips`'s own
    // doc comment for the full reasoning (spec 025 review: nothing verified
    // an outbound-triggering request's real sender before this existed).
    // Resolved once, here, not per-request. Only meaningful in trunk mode
    // (`bindings.is_none()`) with outbound enabled; empty otherwise.
    let trunk_source_ips: Vec<std::net::IpAddr> = if bindings.is_none() && config.outbound.enabled {
        use std::net::ToSocketAddrs;
        match (config.sip.server.as_str(), config.sip.port).to_socket_addrs() {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(e) => {
                tracing::warn!(
                    server = %config.sip.server,
                    error = %e,
                    "outbound: could not resolve the trunk server's address; \
                     dial-out requests will be refused until this succeeds"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // `Account::register` only *initiates* the REGISTER — it does not mean the
    // PBX accepted it. Confirm it did (or report the denial) before treating
    // the outbound bridge leg as usable; assuming success is how a `403` after
    // repeated auth attempts left the bridge "registered" while no call could
    // be placed. On denial we degrade rather than exit (mirroring the IMS
    // renewal loop): PJSUA keeps re-registering on its own timer, so we hold
    // here — with the shared flag cleared so the carrier half fast-declines and
    // status reads `can_answer=false` — and proceed the moment the PBX accepts.
    // Skipped entirely in server mode: there is no PBX whose acceptance could
    // be confirmed, and the local account never sends a REGISTER at all.
    if !config.sip_server.enabled {
        use std::sync::atomic::Ordering;
        let mut backoff = PBX_REG_INITIAL_BACKOFF;
        loop {
            match account.wait_registered(PBX_REG_CONFIRM_TIMEOUT) {
                Ok(()) => {
                    if let Some(flag) = &pbx_registered {
                        flag.store(true, Ordering::SeqCst);
                    }
                    tracing::info!(
                        server = %config.sip.server,
                        port = config.sip.port,
                        agent = agent_label,
                        "registered to PBX"
                    );
                    break;
                }
                Err(e) => {
                    if let Some(flag) = &pbx_registered {
                        flag.store(false, Ordering::SeqCst);
                    }
                    tracing::error!(
                        error = %e,
                        server = %config.sip.server,
                        agent = agent_label,
                        retry_in_secs = backoff.as_secs(),
                        "PBX registration not confirmed — the outbound bridge leg is \
                         unavailable until the PBX accepts it"
                    );
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(PBX_REG_MAX_BACKOFF);
                    // Re-send the REGISTER ourselves: PJSUA may not retry a 403
                    // on its own, so without this the account would never
                    // recover once the registrar starts accepting again.
                    account.trigger_registration();
                }
            }
        }
    }

    // One SIP identity/registration for every line (the spec's own
    // Assumptions section) — what varies per line below is only which
    // veth-peer address/control-port listener accepted the connection.
    tracing::info!(
        line_count = lines.len(),
        lines = ?lines.iter().map(|l| l.card_id.clone()).collect::<Vec<_>>(),
        agent = agent_label,
        "resolved lines"
    );

    // Keyed by card_id (specs/013-multi-card-vowifi FR-017) — replaces the
    // single-line `RecentCalls` instance with one per line, sharing one lock
    // since call volume across a handful of lines never contends on it.
    let recent_calls: Arc<Mutex<HashMap<String, RecentCalls>>> = Arc::new(Mutex::new(
        lines
            .iter()
            .map(|l| (l.card_id.clone(), RecentCalls::new(RECENT_CALLS_CAPACITY)))
            .collect(),
    ));

    // Agent B (this process), not Agent A, owns the actual Discord post for a
    // relayed SIP `MESSAGE` (see `ControlMessage::SmsReceived` docs): it has
    // the `[sms]` webhook config and LAN/Internet reachability, whereas Agent
    // A's netns is IMS-tunnel-only. Each line's accept loop is otherwise
    // synchronous (`std::thread`, no async runtime), so a small runtime is
    // built just to fire off the async `DiscordClient::forward_sms` call
    // without blocking a loop from accepting its next connection.
    let discord_client = build_discord_client(config);
    let sms_runtime = Runtime::new()
        .map_err(|e| BridgeError::Ims(format!("failed to build SMS-forwarding runtime: {e}")))?;
    // Same `[sms].db_path` sqlite file the circuit-switched daemon writes to
    // (WAL mode, see `store::schema`, is exactly what makes two independent
    // processes safely sharing one file work) — so VoWiFi SMS lands in the
    // same `sms` table/history as AT-command SMS, not a separate store.
    let store = StoreHandle::open(Path::new(&config.sms.db_path))
        .map_err(|e| BridgeError::Ims(format!("failed to open SMS store: {e}")))?;

    // Guards `Account::set_identity` + the `Call::make` that reads it back
    // in `bridge_call`, below. Every line's thread closes over the same
    // `&Account` (one shared identity, not one per line), and rewriting
    // that identity then placing a call from it are two separate PJSUA
    // calls with no atomicity between them — without this lock, two lines
    // ringing at once can interleave: line A sets its caller's identity,
    // line B overwrites it with its own before line A's `Call::make` runs,
    // and line A's INVITE goes out carrying line B's caller ID (found in
    // review — Greptile, PR #24).
    let call_placement_lock: Mutex<()> = Mutex::new(());

    // One accept-loop thread per line, all sharing the one endpoint/account/
    // Discord client/store/runtime above — `std::thread::scope` blocks until
    // every thread finishes, which in practice is never (each loops forever
    // like the pre-multi-card single loop did), so this call never returns
    // in normal operation, matching today's behavior for the N=1 case.
    std::thread::scope(|scope| {
        // Outbound calling (specs/025-outbound-calling): this process's
        // `account` (`Account::local` in SIP server mode, or the classic
        // PBX-trunk registration otherwise) is the one that would receive
        // an outbound-triggering INVITE when this half owns the SIP side —
        // exactly the arbitration the registrar already uses (spec 024,
        // research.md R-003). One thread for the whole process, not per
        // line: the account/endpoint are shared, same as the registrar.
        if config.outbound.enabled {
            let endpoint = &endpoint;
            let account = &account;
            let lines_ref = &lines;
            let bindings_ref = bindings.as_ref();
            let trunk_source_ips_ref = &trunk_source_ips;
            // This process serves no `/metrics` of its own — same reason
            // the SIP-server-mode gauges above are reported rather than
            // exported directly (spec 024). `record_transport`'s existing
            // Sip/VolteSip mapping (see the SMS reporter below) keeps
            // VoLTE's outbound attempts distinguishable from Wi-Fi's.
            let outbound_agent_kind = match record_transport {
                crate::store::Transport::Volte => AgentKind::VolteSip,
                _ => AgentKind::Sip,
            };
            let outbound_reporter = Reporter::spawn(
                config.control.socket_path.clone(),
                outbound_agent_kind,
                "outbound".to_string(),
                Duration::from_secs(config.metrics.agent_report_interval_seconds),
            );
            scope.spawn(move || {
                run_outbound_listener(
                    endpoint,
                    account,
                    lines_ref,
                    bindings_ref,
                    trunk_source_ips_ref,
                    &outbound_reporter,
                )
            });
        }

        for line in &lines {
            let endpoint = &endpoint;
            let account = &account;
            let bindings = bindings.as_ref();
            let call_placement_lock = &call_placement_lock;
            let recent_calls = Arc::clone(&recent_calls);
            let discord_client = &discord_client;
            let sms_runtime = &sms_runtime;
            let store_tx = store.sender();
            let card_id = line.card_id.clone();
            let line_msisdn = line.msisdn.clone();
            let listen_addr = (line.veth_peer_addr.clone(), line.control_port);
            let leg_addr = line.veth_local_addr.clone();
            let leg_port = line.sip_leg_port;
            scope.spawn(move || {
                run_line_listener(
                    listen_addr,
                    &card_id,
                    &leg_addr,
                    leg_port,
                    record_transport,
                    endpoint,
                    account,
                    bindings,
                    call_placement_lock,
                    config,
                    &recent_calls,
                    discord_client,
                    sms_runtime,
                    store_tx,
                    line_msisdn.as_deref(),
                );
            });
        }
    });
    Ok(())
}

/// How often to poll for an inbound INVITE this process's account accepted.
/// Independent of any of `run_line_listener`'s per-line cadences — this is
/// process-wide, matching the shared `endpoint`/`account` it polls.
const OUTBOUND_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long a single poll-read blocks while waiting for Agent A's next
/// attempt-phase message before coming up for air to check whether our own
/// caller has hung up (specs/029-interruptible-origination-wait). Short so a
/// mid-attempt hangup is noticed within one tick; the overall wait is still
/// bounded by `CALL_ATTEMPT_TIMEOUT`, which this does not change. Matches the
/// cadence `PBX_RING_POLL_INTERVAL` already uses to watch an inbound call's
/// PBX leg for the same class of hangup.
const ATTEMPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long to wait to connect to a line's Agent A and get its first reply
/// — either `CallFailed` ("busy", no carrier round trip) or `CallAttempting`
/// (committed; a real `CallPlaced`/`CallFailed` follows, waited for
/// separately via `CALL_ATTEMPT_TIMEOUT`). Short by design: this phase only
/// covers "is the process even reachable and idle," never a carrier round
/// trip. Must comfortably exceed Agent A's own `ims::agent::IDLE_POLL_INTERVAL`
/// (1s) — that bounds how long a `PlaceCall` can sit in Agent A's channel
/// before its dispatch loop notices it and replies at all (found live,
/// specs/025-outbound-calling T072, back when that interval was 30s: this
/// timeout gave up before Agent A ever got around to acking). Checked
/// directly in `place_call_timeout_exceeds_agent_as_idle_poll` below, same
/// cross-process reasoning as `CALL_ATTEMPT_TIMEOUT`.
const PLACE_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait for the definitive `CallPlaced`/`CallFailed` once a line
/// has acked an attempt with `CallAttempting`. Must comfortably exceed
/// Agent A's own `ims::agent::OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT`
/// (15s + 60s) — a real carrier call can legitimately take that whole
/// window to ring and answer, plus a little more for the veth handoff
/// right after. Checked against those constants directly in
/// `call_attempt_timeout_exceeds_agent_as_invite_wait` below rather than
/// computed from them, since Agent A runs as a separate OS process (a
/// `vowifi-ims-agent` subprocess in its own netns) even though it's the
/// same compiled binary.
const CALL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(90);

/// Polls for an inbound INVITE this process's account (`Account::local` in
/// SIP server mode, or the classic PBX-trunk registration otherwise)
/// accepted, and places it on whichever line is idle.
///
/// Tries this process's own configured VoWiFi/VoLTE `lines`, each over
/// `contracts/agent-outbound-protocol.md`'s `PlaceCall`/`CallAttempting`/
/// `CallPlaced`/`CallFailed` sequence — a "busy" reply costs no carrier
/// round trip (Agent A's `dispatch_loop` answers it before ever touching
/// the carrier transport), so trying several lines in sequence is cheap;
/// once a line acks with `CallAttempting` it has committed to a real
/// carrier attempt, so `try_place_on_line` stops treating replies as
/// quick and switches to a much longer wait (`CALL_ATTEMPT_TIMEOUT`).
///
/// **No circuit-switched fallback** (specs/025-outbound-calling review):
/// this process's `Endpoint`/ALSA audio path is entirely separate from the
/// daemon's own CS/modem audio path, and no cross-process media bridge
/// connects them — dispatching a CS dial from here used to answer the
/// caller `200 OK` while producing dead air, metrics claiming `placed`. A
/// call refused here because every VoWiFi/VoLTE line is busy/unregistered
/// is refused outright (`503`, `RefusedNoIdleLine`), not retried elsewhere.
/// **Simplification, not strict FR-007 no-preference**: lines are tried in
/// the order `lines` lists them — a real (if arbitrary) ordering, not the
/// "no preference across every process simultaneously" FR-007 describes;
/// true unordered arbitration across processes would need a bigger
/// mechanism than this pass builds.
///
/// Handles at most one VoWiFi/VoLTE-originated call at a time: once one is
/// placed, this loop stops polling for new ones and instead services the
/// active call's control connection (`service_active_outbound_call`) each
/// tick, until it ends — the same single-call-at-a-time model
/// `ims::agent::dispatch_loop` already uses on Agent A's side, chosen for
/// the same reason rather than adding real concurrency here (`pjsua_safe`'s
/// `Endpoint`/`Account` don't implement `Clone` — they're thin handles onto
/// a process-global PJSUA singleton with their own `Drop` teardown — so a
/// second, independently-scheduled call would need actual shared ownership,
/// not just a second thread).
///
/// Runs forever on its own thread (see `run_telephony_side`); never returns
/// in normal operation.
fn run_outbound_listener(
    endpoint: &Endpoint,
    account: &Account,
    lines: &[RuntimeLine],
    // `Some` in SIP server mode: verify a dial-out request's real source
    // against the phones currently registered here — the same check the
    // registrar's own redirect decision makes (`find_by_source`). `None` in
    // trunk mode, where `trunk_source_ips` is the check instead. Exactly one
    // of the two is ever non-empty/`Some` (spec 025 review: nothing verified
    // an outbound-triggering request's real sender at all before this
    // existed — this account's port listens on every interface).
    bindings: Option<&Arc<crate::sip::server::BindingStore>>,
    trunk_source_ips: &[std::net::IpAddr],
    reporter: &Reporter,
) {
    let mut active: Option<ActiveOutboundCall> = None;
    'outer: loop {
        std::thread::sleep(OUTBOUND_POLL_INTERVAL);

        if let Some(ac) = active.as_mut() {
            if service_active_outbound_call(ac) {
                let mut ac = active.take().expect("just matched Some");
                endpoint.unpair_call(ac.call.call_id());
                let _ = ac.call.hangup();
                let _ = ac.veth_call.hangup();
            }
            // Refuse (don't queue) anything that arrives while busy —
            // otherwise it sits in PJSUA's incoming-call queue until this
            // call ends, then gets dialed for a caller who may have given
            // up minutes ago (specs/025-outbound-calling review).
            if let Some((_, stale_call_id, _)) = endpoint.poll_incoming_call() {
                let mut stale = Call::from_id(stale_call_id, CallState::Incoming);
                let _ = stale.answer(503);
                report_outbound(reporter, OutboundAttemptOutcome::RefusedNoIdleLine);
            }
            continue;
        }

        let Some((_, call_id, source_addr)) = endpoint.poll_incoming_call() else {
            continue;
        };
        let mut call = Call::from_id(call_id, CallState::Incoming);

        // This account's port listens on every interface, and nothing
        // upstream has checked who actually sent this INVITE — verify the
        // real transport-level source before it can reach a real carrier
        // call at all (found in review; see this function's own doc
        // comment on `bindings`/`trunk_source_ips`).
        let trusted = match bindings {
            Some(bindings) => bindings
                .find_by_source(source_addr, std::time::Instant::now())
                .is_some(),
            None => trunk_source_ips.contains(&source_addr.ip()),
        };
        if !trusted {
            tracing::warn!(call_id, %source_addr, "outbound: refusing a dial-out request from an untrusted source");
            let _ = call.answer(403);
            report_outbound(reporter, OutboundAttemptOutcome::RefusedInvalidDestination);
            continue;
        }

        let Some(destination) = call.request_destination() else {
            tracing::warn!(
                call_id,
                "outbound: could not determine a destination for this call, refusing"
            );
            let _ = call.answer(400);
            report_outbound(reporter, OutboundAttemptOutcome::RefusedInvalidDestination);
            continue;
        };

        if crate::sip::outbound::validate_destination(&destination).is_err() {
            tracing::warn!(destination = %destination, "outbound: invalid destination, refusing");
            let _ = call.answer(484);
            report_outbound(reporter, OutboundAttemptOutcome::RefusedInvalidDestination);
            continue;
        }

        let wire_call_id = format!("out-{call_id}");
        for line in lines {
            match try_place_on_line(
                endpoint,
                account,
                line,
                &wire_call_id,
                &destination,
                &mut call,
            ) {
                PlaceCallOutcome::Placed(control, early_veth) => {
                    let bridge_result = match early_veth {
                        Some(veth_call) => {
                            finalize_paired_outbound_leg(endpoint, &mut call, veth_call)
                        }
                        None => bridge_outbound_leg(endpoint, account, &mut call, line),
                    };
                    match bridge_result {
                        Ok(veth_call) => {
                            tracing::info!(destination = %destination, card_id = %line.card_id, "outbound call placed over VoWiFi/VoLTE");
                            report_outbound(reporter, OutboundAttemptOutcome::Placed);
                            active = Some(ActiveOutboundCall {
                                call,
                                veth_call,
                                call_id: wire_call_id.clone(),
                                control,
                                pending_line: String::new(),
                            });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "outbound: failed to bridge the veth leg after the carrier leg was placed");
                            // Every other terminal path in this loop
                            // answers the phone/PBX leg with a status
                            // code; this one must too, or it's left
                            // ringing until its own timeout.
                            let _ = call.answer(503);
                            report_outbound(
                                reporter,
                                OutboundAttemptOutcome::RefusedNetworkFailure,
                            );
                        }
                    }
                    // Either way this line was tried and the attempt is
                    // over — don't fall through to the refusal below, and
                    // don't let the borrow checker see a possible later
                    // use of the just-moved `call`, which a plain `break`
                    // + flag check can't prove never happens (the move is
                    // inside this `for` loop; a labeled `continue` on the
                    // outer loop is a real, not just provable, guarantee).
                    continue 'outer;
                }
                PlaceCallOutcome::Unavailable(e) => {
                    tracing::debug!(card_id = %line.card_id, error = %e, "outbound: line unavailable, trying next");
                }
                PlaceCallOutcome::Abandoned => {
                    // Our own caller hung up mid-attempt (specs/029). Agent A
                    // has already been told to CANCEL the carrier leg, and the
                    // phone leg is `Disconnected`, so there is nothing to
                    // answer. Like a committed failure, this ends the whole
                    // request — trying another line would ring a destination
                    // for a caller who is gone (FR-004).
                    tracing::info!(destination = %destination, card_id = %line.card_id, "outbound: caller abandoned the call during the attempt; not trying another line");
                    report_outbound(reporter, OutboundAttemptOutcome::CallerAbandoned);
                    continue 'outer;
                }
                PlaceCallOutcome::Committed(reason) => {
                    // FR-009a: the carrier already answered for this
                    // destination — trying another line would ring it
                    // again for a call it just refused. Stop here with
                    // the carrier's own status code when we have one
                    // (FR-012's progress table) and a properly
                    // distinguished outcome (SC-005), rather than a
                    // blanket 503/`RefusedNetworkFailure` for every
                    // post-commitment failure regardless of cause.
                    let (code, outcome) = outbound_outcome_for_committed_failure(&reason);
                    tracing::warn!(destination = %destination, card_id = %line.card_id, reason = %reason, code, "outbound: carrier rejected the call; not trying another line (FR-009a)");
                    let _ = call.answer(code);
                    report_outbound(reporter, outcome);
                    continue 'outer;
                }
            }
        }

        // No circuit-switched fallback here (specs/025-outbound-calling
        // review): the CS modem lives in the daemon's own process, in its
        // own `SipBridge`/ALSA-device audio path, entirely separate from
        // this process's PJSUA `Endpoint`. Dispatching `ControlCmd::Dial`
        // to the daemon used to place a real ATD and answer `call` with
        // `200 OK` regardless — but nothing ever connected the two
        // processes' audio: the caller got a connected call and dead air,
        // while metrics recorded it as `placed`. Refusing outright is
        // honest about what this process can actually deliver; a real
        // fix needs a cross-process media bridge as substantial as the
        // Agent A/B veth link this feature already relies on for
        // VoWiFi/VoLTE, not a shortcut through the control socket.
        tracing::warn!(destination = %destination, "outbound: no VoWiFi/VoLTE line available, refusing (no cross-process CS audio bridge exists)");
        let _ = call.answer(503);
        report_outbound(reporter, OutboundAttemptOutcome::RefusedNoIdleLine);
    }
}

/// Reports one outbound attempt outcome over the agent-reporting channel —
/// this process serves no `/metrics` of its own (specs/025-outbound-calling
/// T071; found live: the counter was registering in the wrong process's
/// `REGISTRY` and never appearing on the daemon's endpoint, the same class
/// of bug spec 024's `sip_server_bindings` fix already addressed).
fn report_outbound(reporter: &Reporter, outcome: OutboundAttemptOutcome) {
    reporter.report(
        AgentState::default(),
        vec![ObservedEvent::OutboundAttempt { outcome }],
    );
}

/// What happened trying to place a call on one line.
enum PlaceCallOutcome {
    /// Committed and the carrier accepted it — the call is up. Carries the
    /// still-open connection to Agent A (see `try_place_on_line`'s doc), and
    /// — when `CallEarlyMedia` fired earlier in this same attempt
    /// (specs/037-p-early-media) — the veth `Call` already paired to the
    /// phone/PBX leg, so the caller only needs to finalize it
    /// (`finalize_paired_outbound_leg`) rather than pair a second one.
    Placed(std::io::BufReader<TcpStream>, Option<Call>),
    /// Never reached the carrier at all — busy, unreachable, or a
    /// malformed reply. Cheap to have tried; the caller should move on to
    /// the next line.
    Unavailable(String),
    /// Reached the carrier, which answered — a rejection, or (rarely) a
    /// purely local failure discovered after the carrier already accepted
    /// the call. Either way, FR-009a: this is the *destination's* answer
    /// (or an already-committed carrier leg), not a reason to keep hunting
    /// for a working line — trying the next line would ring the same
    /// destination again for a call it just refused, or leave a second,
    /// truly-answered carrier leg orphaned. Found live
    /// (specs/025-outbound-calling review): the two cases used to be the
    /// same `Err(String)`, so a rejected destination got redialed once per
    /// remaining line before finally being refused. The caller must stop
    /// and refuse the whole request with this reason, never try another
    /// line.
    Committed(String),
    /// Our own originating caller hung up before the call connected
    /// (specs/029-interruptible-origination-wait). Agent A has been told to
    /// CANCEL the carrier attempt; there is no phone leg left to answer (it is
    /// already `Disconnected`), and — like `Committed` — trying another line
    /// would only ring a destination for a caller who is already gone.
    Abandoned,
}

/// Reads from a short-timeout control connection, returning a parsed message
/// only once a whole newline-terminated line has arrived. A read timeout
/// (`WouldBlock`/`TimedOut`) yields `Ok(None)` and leaves any partial bytes in
/// `pending_line` for the next call — the specs/029 R7 hazard: `read_msg`
/// allocates a fresh `String` per read, so a message split across a poll
/// boundary would be lost. This is the same carried-buffer discipline
/// `service_active_outbound_call` already uses for an established call.
/// A closed connection is surfaced as `UnexpectedEof`.
fn poll_control_line<R: std::io::BufRead>(
    reader: &mut R,
    pending_line: &mut String,
) -> std::io::Result<Option<ControlMessage>> {
    match reader.read_line(pending_line) {
        Ok(0) => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "control connection closed",
        )),
        // `read_line` only returns `Ok(n>0)` once it has consumed a newline
        // (or hit EOF mid-line, the rare no-newline case we keep buffering).
        Ok(_) if pending_line.ends_with('\n') => {
            let parsed = serde_json::from_str::<ControlMessage>(pending_line.trim());
            pending_line.clear();
            parsed.map(Some).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, format!("parse error: {e}"))
            })
        }
        Ok(_) => Ok(None),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Tears down a veth leg that `CallEarlyMedia` already paired but that
/// never reached a `CallPlaced` finalization — used by every non-`Placed`
/// exit from `try_place_on_line`'s poll loop once early media may have
/// fired (specs/037-p-early-media). A no-op when `early_veth` is `None`,
/// the common case where no early media happened for this attempt.
fn abandon_early_veth(endpoint: &Endpoint, call: &Call, early_veth: Option<Call>) {
    if let Some(mut veth_call) = early_veth {
        endpoint.unpair_call(call.call_id());
        let _ = veth_call.hangup();
    }
}

/// On success, returns the still-open connection to Agent A — it must stay
/// open for the whole call (Agent A expects to send `CallEnded` on it when
/// the carrier hangs up, and we need it to tell Agent A when the phone leg
/// hangs up first). Found live (specs/025-outbound-calling T072 pass 7):
/// the original version dropped the connection the moment it got
/// `CallPlaced` — Agent A's next read on its end saw an immediate EOF,
/// treated it as "Agent B's control connection dropped mid-call", and tore
/// the just-bridged call down again within microseconds. The conference
/// bridge was wired correctly and the relay was running; there was simply
/// nothing left alive downstream by the time either had anything to carry.
fn try_place_on_line(
    endpoint: &Endpoint,
    account: &Account,
    line: &RuntimeLine,
    call_id: &str,
    destination: &str,
    call: &mut Call,
) -> PlaceCallOutcome {
    let addr = match format!("{}:{}", line.veth_local_addr, AGENT_A_STATUS_PORT).parse() {
        Ok(a) => a,
        Err(e) => return PlaceCallOutcome::Unavailable(format!("invalid address: {e}")),
    };
    let mut stream = match TcpStream::connect_timeout(&addr, PLACE_CALL_TIMEOUT) {
        Ok(s) => s,
        Err(e) => return PlaceCallOutcome::Unavailable(format!("connect failed: {e}")),
    };
    if let Err(e) = stream.set_read_timeout(Some(PLACE_CALL_TIMEOUT)) {
        return PlaceCallOutcome::Unavailable(format!("set_read_timeout failed: {e}"));
    }
    if let Err(e) = write_msg(
        &mut stream,
        &ControlMessage::PlaceCall {
            call_id: call_id.to_string(),
            destination: destination.to_string(),
        },
    ) {
        return PlaceCallOutcome::Unavailable(e);
    }

    let mut reader = std::io::BufReader::new(stream);
    match read_msg(&mut reader) {
        // Committed: Agent A is not busy and is about to touch the carrier
        // transport for real. From here on a reply can legitimately take as
        // long as Agent A's own carrier-INVITE wait, so switch to the much
        // longer `CALL_ATTEMPT_TIMEOUT` rather than keep using
        // `PLACE_CALL_TIMEOUT` (found live, specs/025-outbound-calling
        // T072: with one short timeout for both phases, this line gave up
        // and moved on while the carrier was still ringing, and the carrier
        // went on to answer a call nobody was listening for).
        Ok(ControlMessage::CallAttempting { .. }) => {}
        Ok(ControlMessage::CallFailed { reason, .. }) => {
            return PlaceCallOutcome::Unavailable(reason)
        }
        Ok(other) => return PlaceCallOutcome::Unavailable(format!("unexpected reply: {other:?}")),
        Err(e) => return PlaceCallOutcome::Unavailable(e),
    }
    // Poll rather than block from here on (specs/029): a single blocking read
    // with `CALL_ATTEMPT_TIMEOUT` left our own caller's mid-attempt hangup
    // unnoticed for the whole ~80s carrier wait, so no CANCEL ever reached the
    // carrier and it went on ringing a destination for a caller who had left.
    // The read timeout drops to a short poll interval; `CALL_ATTEMPT_TIMEOUT`
    // keeps its value but is now an overall *deadline* (FR-015), and
    // `CallRinging` remains non-terminal — Agent A's own carrier-wait deadline
    // sits comfortably inside that budget regardless of how many polls it takes.
    if let Err(e) = reader
        .get_ref()
        .set_read_timeout(Some(ATTEMPT_POLL_INTERVAL))
    {
        return PlaceCallOutcome::Unavailable(format!("set_read_timeout failed: {e}"));
    }
    let deadline = Instant::now() + CALL_ATTEMPT_TIMEOUT;
    let mut pending_line = String::new();
    // Set once `CallEarlyMedia` fires (specs/037-p-early-media) — the veth
    // leg is already paired and the phone/PBX leg already answered `183`.
    // `CallPlaced` then only needs to finalize it (`Some`), rather than
    // pair a fresh one as it does when no early media ever happened
    // (`None`, the pre-existing path, unchanged).
    let mut early_veth: Option<Call> = None;
    loop {
        match poll_control_line(&mut reader, &mut pending_line) {
            Ok(Some(ControlMessage::CallRinging { .. })) => {
                // Only when early media hasn't already put the caller on an
                // audio-bearing `183` — re-answering `180` after that would
                // downgrade a real early-media dialog to a silent one for no
                // reason (contract: CallRinging and CallEarlyMedia are
                // independent one-shot flags for the same attempt).
                if early_veth.is_none() {
                    let _ = call.answer(180);
                }
            }
            Ok(Some(ControlMessage::CallEarlyMedia { .. })) => {
                if early_veth.is_none() {
                    match pair_veth_leg(endpoint, account, call, line) {
                        Ok(veth_call) => {
                            if let Err(e) = call.answer(183) {
                                tracing::warn!(call_id, error = %e, "outbound: early-media answer(183) failed; continuing without it (FR-006)");
                                endpoint.unpair_call(call.call_id());
                                let mut veth_call = veth_call;
                                let _ = veth_call.hangup();
                            } else {
                                early_veth = Some(veth_call);
                            }
                        }
                        Err(e) => {
                            // FR-006: early media is additive — if pairing
                            // fails, the attempt proceeds exactly as it
                            // would have if `CallEarlyMedia` never arrived,
                            // not as a call failure.
                            tracing::warn!(call_id, error = %e, "outbound: early-media pairing failed; continuing without it (FR-006)");
                        }
                    }
                }
            }
            Ok(Some(ControlMessage::CallPlaced { .. })) => {
                // Short from here on: this same connection is now polled
                // once per `OUTBOUND_POLL_INTERVAL` tick for the rest of
                // the call (`service_active_outbound_call`), not waited on
                // for a single long reply.
                if let Err(e) = reader
                    .get_ref()
                    .set_read_timeout(Some(OUTBOUND_POLL_INTERVAL))
                {
                    abandon_early_veth(endpoint, call, early_veth.take());
                    return PlaceCallOutcome::Committed(format!("set_read_timeout failed: {e}"));
                }
                return PlaceCallOutcome::Placed(reader, early_veth.take());
            }
            Ok(Some(ControlMessage::CallFailed { reason, .. })) => {
                abandon_early_veth(endpoint, call, early_veth.take());
                return PlaceCallOutcome::Committed(reason);
            }
            Ok(Some(other)) => {
                abandon_early_veth(endpoint, call, early_veth.take());
                return PlaceCallOutcome::Committed(format!("unexpected reply: {other:?}"));
            }
            // Nothing complete arrived this tick — fall through to the checks.
            Ok(None) => {}
            Err(e) => {
                abandon_early_veth(endpoint, call, early_veth.take());
                return PlaceCallOutcome::Committed(format!("control read failed: {e}"));
            }
        }

        // Watch our own leg: if the caller hung up while the carrier is still
        // being reached, tell Agent A to abandon the pending INVITE (which
        // sends the CANCEL) and stop — the caller is gone, so no phone-leg
        // answer is owed and no other line should be tried (FR-003, FR-004).
        if call.poll_state() == CallState::Disconnected {
            tracing::info!(
                call_id,
                "outbound: caller hung up during the attempt; telling Agent A to abandon it"
            );
            let _ = write_msg(
                reader.get_mut(),
                &ControlMessage::CallEnded {
                    call_id: call_id.to_string(),
                    reason: control::reason::CALLER_HANGUP.to_string(),
                },
            );
            abandon_early_veth(endpoint, call, early_veth.take());
            return PlaceCallOutcome::Abandoned;
        }

        if Instant::now() >= deadline {
            abandon_early_veth(endpoint, call, early_veth.take());
            return PlaceCallOutcome::Committed(
                "timed out waiting for the carrier attempt to resolve".to_string(),
            );
        }
    }
}

/// Pulls a leading 3-digit SIP status code off a `CallFailed`/`Committed`
/// reason string, when there is one — `ims::agent::fail`'s non-2xx-final-
/// response call site formats its reason as `"{status} {resp.reason}"`
/// (e.g. `"486 Busy Here"`), the one point in the whole outbound path with
/// a real carrier status to report; every other failure reason is free
/// text with no such prefix (a bind failure, a timeout, ...), which this
/// correctly reads as "no carrier status available." Used to answer the
/// phone/PBX leg with the carrier's own code instead of a blanket `503`
/// (specs/025-outbound-calling review, FR-012's progress table).
fn carrier_status_from_reason(reason: &str) -> Option<u32> {
    let code = reason.split(' ').next()?;
    if code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit()) {
        code.parse().ok()
    } else {
        None
    }
}

/// What to answer the phone/PBX leg with, and which outcome to report, for
/// a `PlaceCallOutcome::Committed` failure. Distinguishes "genuinely rang
/// out, nobody ever answered or declined" (`ims::agent`'s
/// `reason::CARRIER_TIMEOUT` marker, or an explicit carrier `480
/// Temporarily Unavailable`) from every other post-commitment failure —
/// SC-005 wants these distinguishable from logs and metrics alone, but
/// `OutboundAttemptOutcome::Unanswered` existed on the wire with nothing
/// ever reporting it (specs/025-outbound-calling review).
fn outbound_outcome_for_committed_failure(reason: &str) -> (u32, OutboundAttemptOutcome) {
    if reason.starts_with(control::reason::CARRIER_TIMEOUT) {
        return (480, OutboundAttemptOutcome::Unanswered);
    }
    match carrier_status_from_reason(reason) {
        Some(480) => (480, OutboundAttemptOutcome::Unanswered),
        // Only a genuine failure code (4xx/5xx/6xx) is safe to hand
        // straight to `Call::answer`. This is a *failure* path — a carrier
        // `202`/`183` reaching here (an unexpected non-final or 2xx status
        // landing in `CallFailed.reason`) is not something a 2xx answer
        // should ever announce, and a `3xx` needs a `Contact` header this
        // call site has no way to supply, so `Call::answer(302)` would send
        // a broken redirect. Fall back to a plain `503` rather than pass
        // either through (specs/025-outbound-calling review).
        Some(code) if (400..700).contains(&code) => {
            (code, OutboundAttemptOutcome::RefusedNetworkFailure)
        }
        Some(_) | None => (503, OutboundAttemptOutcome::RefusedNetworkFailure),
    }
}

/// Places this line's veth-side `Call::make` toward Agent A's now-waiting
/// veth listener and conference-bridges it to the already-accepted
/// phone/PBX leg — the same `pjsua_safe::Endpoint::pair_calls` primitive
/// `bridge_call` already uses for inbound (`vowifi/mod.rs`). Does not
/// answer either leg — callers decide what status to answer `call` with
/// (`bridge_outbound_leg` answers `200` immediately; `try_place_on_line`'s
/// `CallEarlyMedia` handling, specs/037-p-early-media, answers `183`
/// instead, before the call is really placed).
///
/// Pair *before* either leg is answered: `answer()` can complete the
/// INVITE transaction and fire this call's media-active callback on a
/// PJSIP worker thread almost immediately (sub-millisecond, found live —
/// specs/025-outbound-calling T072 pass 4). `pair_calls` is documented as
/// safe to call before either call's media is active precisely for this
/// reason; calling it after left a real window where the phone leg's
/// media-active callback ran before `BRIDGE_PAIRS` had this pairing, so it
/// fell through to the sound-device branch instead of the peer-call
/// branch — audio silently went nowhere, no error on either side.
fn pair_veth_leg(
    endpoint: &Endpoint,
    account: &Account,
    call: &mut Call,
    line: &RuntimeLine,
) -> BridgeResult<Call> {
    let veth_uri = format!("sip:agent-a@{}:{}", line.veth_local_addr, line.sip_leg_port);
    let veth_call = Call::make(account, &veth_uri, None, &[])
        .map_err(|e| BridgeError::Ims(format!("veth-side call failed: {e}")))?;
    endpoint.pair_calls(call.call_id(), veth_call.call_id());
    Ok(veth_call)
}

/// `pair_veth_leg` followed by `answer(200)` — the full no-early-media
/// path: `call` was accepted first, the veth leg is placed and paired
/// second, then answered. Returns the veth `Call` on success — the caller
/// must hang it up (and unpair) once the bridged call ends, same as it
/// must for `call`.
fn bridge_outbound_leg(
    endpoint: &Endpoint,
    account: &Account,
    call: &mut Call,
    line: &RuntimeLine,
) -> BridgeResult<Call> {
    let mut veth_call = pair_veth_leg(endpoint, account, call, line)?;
    if let Err(e) = call.answer(200) {
        // The pairing just made above and the veth call `Call::make` just
        // placed would otherwise leak: `Call` has no `Drop`, and a stale
        // `BRIDGE_PAIRS` entry could pair a *future* unrelated call to this
        // dead `veth_call.call_id()`.
        endpoint.unpair_call(call.call_id());
        let _ = veth_call.hangup();
        return Err(BridgeError::Ims(format!("{e}")));
    }
    tracing::info!(
        phone_call_id = call.call_id(),
        veth_call_id = veth_call.call_id(),
        card_id = %line.card_id,
        "outbound: placed and paired both legs"
    );
    Ok(veth_call)
}

/// The `CallPlaced` finalization when `try_place_on_line` already paired
/// the veth leg early (`CallEarlyMedia` fired for this attempt,
/// specs/037-p-early-media) — only `answer(200)` is left to do; pairing
/// already happened, so redoing it would place a second, orphaned veth
/// call. Mirrors `bridge_outbound_leg`'s answer-failure cleanup exactly.
fn finalize_paired_outbound_leg(
    endpoint: &Endpoint,
    call: &mut Call,
    mut veth_call: Call,
) -> BridgeResult<Call> {
    if let Err(e) = call.answer(200) {
        endpoint.unpair_call(call.call_id());
        let _ = veth_call.hangup();
        return Err(BridgeError::Ims(format!("{e}")));
    }
    tracing::info!(
        phone_call_id = call.call_id(),
        veth_call_id = veth_call.call_id(),
        "outbound: answered 200 on an already-paired (early media) leg"
    );
    Ok(veth_call)
}

/// An outbound call in progress: both legs, plus the still-open connection
/// to the Agent A that placed it. Held by `run_outbound_listener` between
/// polling ticks (specs/025-outbound-calling T072 pass 7) — `control` must
/// stay open for the call's whole lifetime, unlike the brief request/reply
/// use `try_place_on_line` makes of it before this point.
struct ActiveOutboundCall {
    call: Call,
    veth_call: Call,
    call_id: String,
    control: std::io::BufReader<TcpStream>,
    /// Carries a message across ticks when `read_line` times out mid-line
    /// (specs/025-outbound-calling review): `read_line` documents that any
    /// bytes it already appended stay in the buffer even when it returns
    /// an error, but a fresh `String::new()` per call throws that partial
    /// data away — and the remainder that arrives on a later tick becomes
    /// an orphaned, unparseable fragment on its own. Reused (not cleared)
    /// across timeouts; only cleared once a complete line has actually
    /// been consumed.
    pending_line: String,
}

/// One non-blocking check of `ac.control` (`OUTBOUND_POLL_INTERVAL` read
/// timeout, set by `try_place_on_line`) plus `ac.call`'s own state.
/// Returns `true` once the call has ended — either side may end it first:
///
/// - Agent A sends `CallEnded` when the carrier hangs up; this just
///   observes it (the caller does the actual `Call::hangup`/`unpair_call`
///   teardown, uniformly for both directions).
/// - `ac.call` reaching `Disconnected` means the phone/PBX leg hung up
///   first; Agent A doesn't know yet, so this sends `CallEnded` to tell it
///   to BYE the carrier — the mirror of `handle_connection`'s inbound
///   teardown loop (`vowifi/mod.rs`), which this deliberately matches.
fn service_active_outbound_call(ac: &mut ActiveOutboundCall) -> bool {
    match ac.control.read_line(&mut ac.pending_line) {
        Ok(0) => {
            tracing::warn!(call_id = %ac.call_id, "outbound: control connection to Agent A lost mid-call");
            true
        }
        Ok(_) => {
            let result = match serde_json::from_str::<ControlMessage>(ac.pending_line.trim()) {
                Ok(ControlMessage::CallEnded { reason, .. }) => {
                    tracing::info!(call_id = %ac.call_id, reason = %reason, "outbound: carrier leg ended, tearing down the phone leg");
                    true
                }
                Ok(other) => {
                    tracing::warn!(call_id = %ac.call_id, message = ?other, "outbound: unexpected message during an active call");
                    false
                }
                Err(e) => {
                    tracing::warn!(call_id = %ac.call_id, error = %e, "outbound: malformed message on the control connection");
                    false
                }
            };
            // A full line was consumed either way (parse failure is not a
            // reason to keep it around) — clear it so the next `read_line`
            // starts a fresh message rather than appending onto this one.
            ac.pending_line.clear();
            result
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            if ac.call.poll_state() == CallState::Disconnected {
                tracing::info!(call_id = %ac.call_id, "outbound: phone leg hung up; telling Agent A to end the carrier leg");
                let _ = write_msg(
                    ac.control.get_mut(),
                    &ControlMessage::CallEnded {
                        call_id: ac.call_id.clone(),
                        reason: control::reason::PBX_HANGUP.to_string(),
                    },
                );
                true
            } else {
                false
            }
        }
        Err(e) => {
            tracing::warn!(call_id = %ac.call_id, error = %e, "outbound: control connection read failed");
            true
        }
    }
}

/// One line's whole accept loop — binds `listen_addr` and handles every
/// connection Agent A opens on it, tagging everything with `card_id`
/// (FR-017). Runs on its own thread (see `run_inner`); a bind failure here
/// is logged and the thread simply exits, leaving the other lines' threads
/// (and this process) running — one line's misconfiguration shouldn't take
/// the whole Agent B process down.
///
/// Owns a `Reporter` scoped to this one line/`card_id`
/// (specs/014-vowifi-metrics-restore): with several lines sharing this one
/// process (specs/013-multi-card-vowifi), a single shared `Reporter` could
/// only ever report on behalf of one fixed module id, so each line gets its
/// own — cheap (a channel plus a background thread) and matches how Agent A
/// naturally gets one per process, one per line, for free.
/// How long to wait between attempts to bind a line's control channel.
const CONTROL_BIND_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// How many consecutive bind failures between log lines, after the first —
/// enough that a genuinely misconfigured address stays visible without filling
/// the log while waiting for a slow carrier's tunnel.
const CONTROL_BIND_LOG_EVERY: u32 = 15;

/// Binds this line's control channel, retrying until it succeeds.
///
/// The address belongs to a veth the supervisor only creates once this line's
/// tunnel is up, which can be minutes after Agent B starts — and lines come up
/// in whatever order their carriers answer, which is not stable across runs.
///
/// A one-shot bind here meant the first line ready won and every slower line
/// was dropped for the whole process lifetime. Observed live with two lines:
/// both binds failed at startup with `EADDRNOTAVAIL`, a restart moments later
/// caught `10.99.0.6`, and `10.99.0.2` stayed permanently unable to receive
/// calls even though its veth appeared seconds afterwards — with only that one
/// already-scrolled-past error line to say so.
///
/// So `EADDRNOTAVAIL` is an expected transient startup condition, not a fatal
/// one. There is no bound on how long a carrier may take, and giving up is
/// exactly the failure being fixed, so this retries indefinitely; the caller
/// already blocks forever in `accept` immediately afterwards.
///
/// Generic over the bind and sleep so the retry policy is testable without a
/// network or a real clock.
fn bind_with_retry<T, E: std::fmt::Display>(
    card_id: &str,
    addr: &str,
    interval: Duration,
    mut bind: impl FnMut() -> Result<T, E>,
    mut sleep: impl FnMut(Duration),
) -> T {
    let mut attempt: u32 = 0;
    loop {
        match bind() {
            Ok(bound) => {
                if attempt > 0 {
                    tracing::info!(
                        card_id = %card_id,
                        addr = %addr,
                        attempt,
                        "control channel bound after retrying"
                    );
                }
                return bound;
            }
            Err(e) => {
                if attempt.is_multiple_of(CONTROL_BIND_LOG_EVERY) {
                    tracing::warn!(
                        card_id = %card_id,
                        addr = %addr,
                        error = %e,
                        attempt,
                        "control channel not bindable yet (this line's veth is \
                         probably not up); retrying"
                    );
                }
                attempt = attempt.saturating_add(1);
                sleep(interval);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_line_listener(
    listen_addr: (String, u16),
    card_id: &str,
    leg_addr: &str,
    leg_port: u16,
    transport: crate::store::Transport,
    endpoint: &Endpoint,
    account: &Account,
    // `Some` in SIP server mode: the phones registered to this process's own
    // registrar, which is where the outbound leg is dialled instead of a PBX.
    bindings: Option<&Arc<crate::sip::server::BindingStore>>,
    call_placement_lock: &Mutex<()>,
    config: &AppConfig,
    recent_calls: &Arc<Mutex<HashMap<String, RecentCalls>>>,
    discord_client: &Option<DiscordClient>,
    sms_runtime: &Runtime,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    line_msisdn: Option<&str>,
) {
    // This same telephony code serves both paths, so the reporter's kind must
    // follow the transport it is bridging: reported as `Sip` the VoLTE
    // bridge's PBX-leg outcomes land under `transport="vowifi"`, making VoLTE
    // and Wi-Fi calls indistinguishable in the one comparison this whole
    // effort exists to make (the same class of bug as specs/017 R15, which
    // fixed the gauges and `CALLS_TOTAL` but not this counter).
    let agent_kind = match transport {
        crate::store::Transport::Volte => AgentKind::VolteSip,
        _ => AgentKind::Sip,
    };
    let reporter = Reporter::spawn(
        config.control.socket_path.clone(),
        agent_kind,
        card_id.to_string(),
        Duration::from_secs(config.metrics.agent_report_interval_seconds),
    );
    reporter.report(
        AgentState {
            pbx_registered: Some(true),
            ..Default::default()
        },
        Vec::new(),
    );

    let addr_str = format!("{}:{}", listen_addr.0, listen_addr.1);
    let listener = bind_with_retry(
        card_id,
        &addr_str,
        CONTROL_BIND_RETRY_INTERVAL,
        || TcpListener::bind((listen_addr.0.as_str(), listen_addr.1)),
        std::thread::sleep,
    );
    tracing::info!(
        card_id = %card_id,
        addr = %listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default(),
        "vowifi-sip-agent listening for Agent A"
    );

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(card_id = %card_id, error = %e, "control channel accept failed");
                continue;
            }
        };
        if let Err(e) = handle_connection(
            stream,
            card_id,
            leg_addr,
            leg_port,
            transport,
            endpoint,
            account,
            bindings,
            call_placement_lock,
            config,
            recent_calls,
            discord_client,
            sms_runtime,
            store_tx.clone(),
            &reporter,
            line_msisdn,
        ) {
            tracing::warn!(card_id = %card_id, error = %e, "error handling Agent A control connection");
        }
    }
}

/// Builds the Discord client used to forward relayed VoWiFi `MESSAGE`s,
/// mirroring `modules::mod::CardPool::new`'s gating: only if SMS monitoring
/// is enabled and a webhook URL is actually configured.
fn build_discord_client(config: &AppConfig) -> Option<DiscordClient> {
    if !config.sms.enabled {
        tracing::info!(
            "SMS monitoring disabled via configuration; VoWiFi SMS will not be forwarded"
        );
        return None;
    }
    if config.sms.discord_webhook_url.expose_secret().is_empty() {
        tracing::info!(
            "SMS forwarding disabled (no webhook URL configured); VoWiFi SMS will not be forwarded"
        );
        return None;
    }
    match DiscordClient::new(
        config.sms.discord_webhook_url.clone(),
        crate::alerts::instance_label(&config.alerts),
    ) {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::error!(error = %e, "failed to create Discord client");
            None
        }
    }
}

/// PJSIP's G.722 codec id. Wideband (16 kHz internally, whatever RFC 3551's
/// historical `G722/8000` rtpmap says), built into pjproject with no external
/// library, and understood by every mainstream PBX without an extra module —
/// which is why it, rather than Opus, is what the PBX leg reaches for.
const G722_CODEC_ID: &str = "G722/16000/1";
/// PJSIP's 16 kHz linear-PCM codec id — uncompressed, and used only on the
/// veth link to Agent A (see `ims::sdp::NegotiatedCodec::L16`).
const L16_16K_CODEC_ID: &str = "L16/16000/1";

/// Make Agent B's two calls offer the codecs a wideband bridge needs: G.722
/// first (what the PBX should pick), and L16/16000 enabled so it appears in the
/// offer at all (what Agent A picks on the veth link, by name — so its low
/// priority here doesn't matter; it only keeps L16 out of the PBX's way).
///
/// Priorities are endpoint-global, and best-effort: a PJSIP build missing
/// either codec just logs a warning and carries on. Nothing here can fail a
/// call — without G.722 the PBX leg falls back to PCMU, and without L16 the
/// veth link does, which is exactly how this bridge behaved before wideband.
fn prioritize_wideband_codecs(endpoint: &Endpoint) {
    for (codec_id, priority) in [(G722_CODEC_ID, 200), (L16_16K_CODEC_ID, 1)] {
        if let Err(e) = endpoint.set_codec_priority(codec_id, priority) {
            tracing::warn!(
                codec = codec_id,
                error = %e,
                "could not set codec priority; this PJSIP build may not have the codec"
            );
        }
    }
    tracing::info!(
        codecs = ?endpoint
            .codecs()
            .iter()
            .filter(|c| c.priority > 0)
            .map(|c| (c.id.clone(), c.priority))
            .collect::<Vec<_>>(),
        "PJSIP codecs offered, in priority order"
    );
}

/// Records a call outcome under `card_id`'s own history — inserts an entry
/// if this is somehow the first time this card_id is seen (shouldn't
/// happen; `recent_calls` is pre-populated from the same line list this
/// listener was spawned from, but a missing entry degrading to "start
/// empty" is safer than losing the record).
///
/// Deliberately does not also touch a metric here: the overall call outcome
/// (answered/missed/failed) is Agent A's to report, not Agent B's — Agent A
/// sees every inbound INVITE, including ones that never reach this far
/// (specs/014-vowifi-metrics-restore, research.md §R3's ownership table).
/// Reporting it again here, from a different vantage point with a
/// differently-shaped vocabulary (`record.outcome`'s free-form
/// `"declined:<reason>"` strings), would both double-count and reintroduce
/// unbounded label cardinality (FR-014) — `record.outcome` is arbitrary text
/// interpolated with an error's `Display` output in the `Err(e)` path above.
fn push_recent_call(
    recent_calls: &Arc<Mutex<HashMap<String, RecentCalls>>>,
    card_id: &str,
    record: CallRecord,
) {
    recent_calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(card_id.to_string())
        .or_insert_with(|| RecentCalls::new(RECENT_CALLS_CAPACITY))
        .push(record);
}

#[allow(clippy::too_many_arguments)]
fn handle_connection(
    stream: TcpStream,
    card_id: &str,
    leg_addr: &str,
    leg_port: u16,
    transport: crate::store::Transport,
    endpoint: &Endpoint,
    account: &Account,
    bindings: Option<&Arc<crate::sip::server::BindingStore>>,
    call_placement_lock: &Mutex<()>,
    config: &AppConfig,
    recent_calls: &Arc<Mutex<HashMap<String, RecentCalls>>>,
    discord_client: &Option<DiscordClient>,
    sms_runtime: &Runtime,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    reporter: &Reporter,
    line_msisdn: Option<&str>,
) -> BridgeResult<()> {
    let mut reader = std::io::BufReader::new(
        stream
            .try_clone()
            .map_err(|e| BridgeError::Ims(format!("failed to clone control connection: {e}")))?,
    );
    let mut writer = stream;

    let msg = read_msg(&mut reader).map_err(BridgeError::Ims)?;
    let (call_id, caller) = match msg {
        ControlMessage::IncomingCall { call_id, caller } => (call_id, caller),
        ControlMessage::StatusQuery => {
            let calls = recent_calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(card_id)
                .map(RecentCalls::snapshot)
                .unwrap_or_default();
            write_msg(&mut writer, &ControlMessage::CallHistoryReply { calls })
                .map_err(BridgeError::Ims)?;
            return Ok(());
        }
        ControlMessage::SmsReceived {
            sender,
            body,
            received_at,
        } => {
            reporter.report(AgentState::default(), vec![ObservedEvent::SmsReceived]);
            forward_vowifi_sms(
                store_tx,
                discord_client,
                sms_runtime,
                card_id.to_string(),
                sender,
                body,
                received_at,
                reporter.clone(),
                transport,
                line_msisdn.map(str::to_string),
            );
            return Ok(());
        }
        other => {
            return Err(BridgeError::Ims(format!(
                "expected IncomingCall, StatusQuery, or SmsReceived as the first message on a control connection, got {other:?}"
            )));
        }
    };
    tracing::info!(card_id = %card_id, call_id = %call_id, caller = %caller, "incoming VoWiFi call signaled by Agent A");
    let started_at = now_unix();

    match bridge_call(
        endpoint,
        account,
        bindings,
        call_placement_lock,
        config,
        &caller,
        leg_addr,
        leg_port,
        line_msisdn.unwrap_or(""),
    ) {
        Ok((mut pbx_call, mut veth_call)) => {
            write_msg(
                &mut writer,
                &ControlMessage::BridgeReady {
                    call_id: call_id.clone(),
                    // Informational only — the real RTP port exchange
                    // happens over the veth-internal SDP dialog
                    // (`ims::agent`'s UAS), not this control channel.
                    veth_rtp_port: 0,
                },
            )
            .map_err(BridgeError::Ims)?;

            // Read Agent A's messages on a thread from here on. While the PBX
            // rings we must still notice a `CallEnded` (the caller gave up) —
            // blocking on the PBX's state alone would leave the extension
            // ringing for the whole timeout after the caller had already hung
            // up.
            let ctrl_rx = spawn_control_reader(reader);

            // The PBX extension is only *ringing* at this point. Agent A holds
            // the carrier in the ringing state (so the network keeps playing
            // ringback to the caller) until we tell it a human actually picked
            // up — answering the carrier the moment the INVITE went out would
            // replace the caller's ringback with dead air.
            match wait_for_pbx_answer(&pbx_call, &ctrl_rx) {
                PbxOutcome::Answered => {
                    tracing::info!(call_id = %call_id, "PBX extension answered");
                    reporter.report(
                        AgentState::default(),
                        vec![ObservedEvent::PbxLegCompleted {
                            outcome: SmsOutcome::Sent,
                        }],
                    );
                    write_msg(
                        &mut writer,
                        &ControlMessage::CallAnswered {
                            call_id: call_id.clone(),
                        },
                    )
                    .map_err(BridgeError::Ims)?;
                }
                outcome => {
                    let reason = outcome.reason();
                    tracing::info!(call_id = %call_id, reason, "PBX leg never answered; declining");
                    reporter.report(
                        AgentState::default(),
                        vec![ObservedEvent::PbxLegCompleted {
                            outcome: SmsOutcome::Failed,
                        }],
                    );
                    let _ = write_msg(
                        &mut writer,
                        &ControlMessage::BridgeFailed {
                            call_id: call_id.clone(),
                            reason: reason.to_string(),
                        },
                    );
                    endpoint.unpair_call(pbx_call.call_id());
                    let _ = pbx_call.hangup();
                    let _ = veth_call.hangup();
                    push_recent_call(
                        recent_calls,
                        card_id,
                        CallRecord {
                            call_id,
                            caller,
                            outcome: format!("declined:{reason}"),
                            started_at,
                            ended_at: Some(now_unix()),
                        },
                    );
                    return Ok(());
                }
            }

            // A hangup can start on either side. Blocking on Agent A alone
            // would miss the PBX extension hanging up first, leaving the caller
            // on a line that is already dead — so watch our own leg too, and
            // tell Agent A when it drops so it can BYE the carrier.
            let end_reason = loop {
                match ctrl_rx.recv_timeout(PBX_RING_POLL_INTERVAL) {
                    Ok(ControlMessage::CallEnded { reason, .. }) => {
                        tracing::info!(call_id = %call_id, reason = %reason, "call ended, tearing down both legs");
                        break reason;
                    }
                    Ok(other) => {
                        tracing::warn!(call_id = %call_id, message = ?other, "unexpected message during an active call");
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        tracing::warn!(call_id = %call_id, "control connection lost mid-call; tearing down anyway");
                        break "control_connection_lost".to_string();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if pbx_call.poll_state() == CallState::Disconnected {
                    tracing::info!(call_id = %call_id, "PBX side hung up; telling Agent A to end the carrier leg");
                    let _ = write_msg(
                        &mut writer,
                        &ControlMessage::CallEnded {
                            call_id: call_id.clone(),
                            reason: control::reason::PBX_HANGUP.to_string(),
                        },
                    );
                    break control::reason::PBX_HANGUP.to_string();
                }
            };
            endpoint.unpair_call(pbx_call.call_id());
            let _ = pbx_call.hangup();
            let _ = veth_call.hangup();
            let _ = write_msg(
                &mut writer,
                &ControlMessage::HangupAck {
                    call_id: call_id.clone(),
                },
            );
            push_recent_call(
                recent_calls,
                card_id,
                CallRecord {
                    call_id,
                    caller,
                    outcome: format!("answered:{end_reason}"),
                    started_at,
                    ended_at: Some(now_unix()),
                },
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(card_id = %card_id, call_id = %call_id, error = %e, "failed to bridge call");
            reporter.report(
                AgentState::default(),
                vec![ObservedEvent::PbxLegCompleted {
                    outcome: SmsOutcome::Failed,
                }],
            );
            write_msg(
                &mut writer,
                &ControlMessage::BridgeFailed {
                    call_id: call_id.clone(),
                    reason: control::reason::PBX_UNREACHABLE.to_string(),
                },
            )
            .map_err(BridgeError::Ims)?;
            push_recent_call(
                recent_calls,
                card_id,
                CallRecord {
                    call_id,
                    caller,
                    outcome: format!("failed:{e}"),
                    started_at,
                    ended_at: Some(now_unix()),
                },
            );
            Ok(())
        }
    }
}

/// Persists a relayed VoWiFi `MESSAGE` and forwards it to Discord, via the
/// same `sms::record_and_forward` the circuit-switched flow's AT-command SMS
/// uses (`modules::mod`'s `BridgeEvent::SmsReceived` handler) — one `sms`
/// table, one forwarding/retry/status-update implementation, regardless of
/// which transport the message arrived on. Runs on `sms_runtime`
/// (`run_inner`'s dedicated small runtime, since this whole accept loop is
/// otherwise synchronous): the connection carrying this message doesn't wait
/// for a reply, so there is nothing to block on here, and blocking the
/// accept loop on Discord's round trip would delay the next inbound call.
#[allow(clippy::too_many_arguments)]
fn forward_vowifi_sms(
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    discord_client: &Option<DiscordClient>,
    sms_runtime: &Runtime,
    card_id: String,
    sender: String,
    body: String,
    received_at: String,
    reporter: Reporter,
    transport: crate::store::Transport,
    phone_number: Option<String>,
) {
    // Records first (status "pending"), forwards second, updates the status
    // after — so a message survives a downstream outage rather than being
    // lost with it (specs/017 FR-029).
    sms::record_and_forward(
        sms_runtime.handle(),
        store_tx,
        discord_client.clone(),
        card_id,
        sender,
        body,
        received_at,
        transport,
        Some(reporter),
        phone_number,
    );
}

/// How long to let the PBX extension ring before giving up. The caller hears
/// ringback for this whole window, so it wants to be a natural ring duration —
/// long enough for someone to walk to the phone, short enough that the carrier
/// doesn't time the call out from its own end first.
const PBX_RING_TIMEOUT: Duration = Duration::from_secs(45);
/// How often to re-check the PBX leg's state while it rings. PJSIP's state is
/// polled rather than pushed (see `Call::poll_state`); 100ms is imperceptible
/// against a human picking up a phone and costs nothing.
const PBX_RING_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// What became of the PBX leg while the caller listened to ringback.
enum PbxOutcome {
    /// A human picked up.
    Answered,
    /// The PBX hung up on us — busy, rejected, or the extension is gone.
    Rejected,
    /// It just rang out.
    NoAnswer,
    /// The caller gave up before anyone picked up. Agent A has already told
    /// the carrier; we only need to stop ringing the extension.
    CallerGone,
}

impl PbxOutcome {
    fn reason(&self) -> &'static str {
        match self {
            // Only ever called on the paths that didn't answer.
            PbxOutcome::Answered => "answered",
            PbxOutcome::Rejected => control::reason::PBX_REJECTED,
            PbxOutcome::NoAnswer => control::reason::PBX_NO_ANSWER,
            PbxOutcome::CallerGone => control::reason::CALLER_CANCELLED,
        }
    }
}

/// Ring the PBX extension until someone answers, the PBX gives up on us, the
/// caller hangs up, or `PBX_RING_TIMEOUT` elapses.
fn wait_for_pbx_answer(pbx_call: &Call, ctrl_rx: &mpsc::Receiver<ControlMessage>) -> PbxOutcome {
    let deadline = Instant::now() + PBX_RING_TIMEOUT;
    while Instant::now() < deadline {
        match pbx_call.poll_state() {
            CallState::Confirmed => return PbxOutcome::Answered,
            CallState::Disconnected => return PbxOutcome::Rejected,
            // Calling/Early — still ringing.
            _ => {}
        }
        // The caller may hang up mid-ring; stop ringing the extension at once
        // rather than making it ring on for the rest of the timeout.
        match ctrl_rx.recv_timeout(PBX_RING_POLL_INTERVAL) {
            Ok(ControlMessage::CallEnded { .. }) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return PbxOutcome::CallerGone
            }
            Ok(other) => {
                tracing::debug!(message = ?other, "ignoring control message while the PBX rings")
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    PbxOutcome::NoAnswer
}

/// Reads Agent A's control messages on a thread so the ring loop can wait on
/// the PBX's state and the control channel at the same time. Mirrors
/// `ims::agent::spawn_control_reader`.
fn spawn_control_reader(
    mut reader: std::io::BufReader<TcpStream>,
) -> mpsc::Receiver<ControlMessage> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        match read_msg(&mut reader) {
            Ok(msg) => {
                if tx.send(msg).is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "Agent A control connection reader stopped");
                return;
            }
        }
    });
    rx
}

/// Places both legs — the PBX-side call (reusing the same destination-URI
/// and caller-ID header logic as the circuit-switched bridge,
/// `crate::sip::SipBridge::compute_destination_uri`/`make_call`, FR-003/
/// FR-011) and the veth-side call back to Agent A's UAS
/// (`crate::ims::agent`, listening on `VETH_SIP_PORT`) — and pairs them via
/// `Endpoint::pair_calls` so their media bridges together once both reach
/// `PJSUA_CALL_MEDIA_ACTIVE` (see `pjsua-safe/src/endpoint.rs`'s
/// `on_call_media_state_cb`).
#[allow(clippy::too_many_arguments)]
fn bridge_call(
    endpoint: &Endpoint,
    account: &Account,
    bindings: Option<&Arc<crate::sip::server::BindingStore>>,
    call_placement_lock: &Mutex<()>,
    config: &AppConfig,
    caller: &str,
    leg_addr: &str,
    leg_port: u16,
    line_number: &str,
) -> BridgeResult<(Call, Call)> {
    // Resolved before either leg is placed: in SIP server mode there may be no
    // phone registered, and finding that out after answering the carrier would
    // leave the caller connected to nothing.
    let pbx_uri = telephony_dest_uri(config, bindings, caller, line_number)?;

    let mut headers: Vec<(&str, &str)> = Vec::new();
    let pai_value;
    if !caller.is_empty() {
        pai_value = format!("\"{caller}\" <tel:{caller}>");
        headers.push(("P-Asserted-Identity", &pai_value));
        headers.push(("X-GSM-Caller-ID", caller));
    }

    // SIP server mode has no per-line Request-URI to carry `line_number`
    // the way the PBX-destination fallback above does (the destination
    // there is always the registered phone's real contact) — see
    // `crate::sip::SipBridge::make_call`'s identical `P-Called-Party-ID`
    // header for the circuit-switched side of this same rule.
    let pcpid_value;
    if bindings.is_some() && !line_number.is_empty() {
        pcpid_value = format!("<{}>", config.sip_server.caller_identity_uri(line_number));
        headers.push(("P-Called-Party-ID", &pcpid_value));
    }

    // SIP server mode: same reasoning as `crate::sip::SipBridge::make_call`
    // — the phone receives this INVITE directly, so it is the `From` (not
    // `P-Asserted-Identity` above, which only a PBX in the middle would
    // read) that must carry the real caller. `set_identity` rewrites the
    // one `Account` every line's thread shares, and `Call::make` right
    // below is what actually reads that identity into the INVITE it
    // builds — the lock has to span both, or another line's thread can set
    // its own caller in between and this call goes out under the wrong
    // name (see `call_placement_lock`'s doc comment in `run`). Not held
    // around the veth-side call further down: nothing reads this account's
    // identity for that leg, and a PJSIP dialog never re-reads it once its
    // INVITE has been sent.
    let pbx_call = {
        let _guard = call_placement_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if bindings.is_some() {
            let id_uri = config.sip_server.caller_identity_uri(caller);
            let display = if caller.is_empty() {
                config.sip.display_name.as_str()
            } else {
                caller
            };
            if let Err(e) = account.set_identity(&id_uri, display) {
                tracing::warn!(
                    error = %e,
                    "sip_server: failed to set this call's caller identity; \
                     phone will show the bridge's own identity instead"
                );
            }
        }
        Call::make(account, &pbx_uri, None, &headers)
            .map_err(|e| BridgeError::Ims(format!("PBX-side call failed: {e}")))?
    };

    let veth_uri = format!("sip:agent-a@{leg_addr}:{leg_port}");
    let veth_call = Call::make(account, &veth_uri, None, &[])
        .map_err(|e| BridgeError::Ims(format!("veth-side call failed: {e}")))?;

    endpoint.pair_calls(pbx_call.call_id(), veth_call.call_id());
    tracing::info!(
        pbx_call_id = pbx_call.call_id(),
        veth_call_id = veth_call.call_id(),
        dest = %pbx_uri,
        "placed and paired both legs"
    );

    Ok((pbx_call, veth_call))
}

/// Where this call's telephony-side leg goes, sharing one rule with the
/// circuit-switched bridge via `crate::sip::target::CallTarget`.
///
/// With a PBX: empty `[bridge].sip_destination` means DID passthrough (dial
/// this line's own number — `line_number`, the `[[vowifi.line]].msisdn` —
/// at the PBX, so a PBX fed by several lines can tell them apart),
/// otherwise the configured fixed extension. In SIP server mode: the
/// registered phone's own contact, which is the only case that can fail.
fn telephony_dest_uri(
    config: &AppConfig,
    bindings: Option<&Arc<crate::sip::server::BindingStore>>,
    caller_did: &str,
    line_number: &str,
) -> BridgeResult<String> {
    let target = match bindings {
        Some(bindings) => crate::sip::target::CallTarget::RegisteredPhone {
            bindings,
            aor: &config.sip_server.ring_aor,
        },
        None => crate::sip::target::CallTarget::Pbx {
            server: &config.sip.server,
            port: config.sip.port,
            sip_destination: &config.bridge.sip_destination,
            line_number,
        },
    };
    target
        .uri_for(caller_did, std::time::Instant::now())
        .map_err(|e| {
            crate::metrics::SIP_SERVER_RING_TARGET_MISSING_TOTAL.inc();
            BridgeError::Ims(e)
        })
}

/// Entry point for the `vowifi-status` subcommand: queries every resolved
/// line's Agent A registration health (`AGENT_A_STATUS_PORT`, reached via
/// that line's own veth-local address) and Agent B's per-line recent call
/// history (that line's own veth-peer address/control port), printing one
/// labeled block per line — FR-018/User Story 3. A query failing for one
/// line is reported for that line only, not fatal to reporting the others
/// (acceptance scenario 1); overall failure means *every* line's queries
/// failed.
pub fn print_status(_config: &VowifiConfig) -> ExitCode {
    let path = lines_file_path();
    let resolution = match discovery::read_line_resolution(&path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    // specs/027-discover-retry-health FR-006: a configured line that never
    // resolved at all must still be reported (the section below), not just
    // silently fail here the way it used to — only bail out this early if
    // there is truly nothing to say about *any* line, configured or not.
    if resolution.lines.is_empty() && resolution.failed.is_empty() {
        eprintln!(
            "error: no VoWiFi lines resolved in {} — run `gsm-sip-bridge discover` first",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let lines = runtime_lines_from_resolution(&resolution);
    let mut any_ok = false;

    for line in &lines {
        println!("Line {} (card {}):", line.index, line.card_id);
        let mut line_ok = true;

        println!("  VoWiFi registration (Agent A):");
        match query_status(&format!("{}:{AGENT_A_STATUS_PORT}", line.veth_local_addr)) {
            Ok(ControlMessage::RegistrationStatusReply {
                state,
                registered_at,
                expires_at,
                last_failure,
                can_answer,
                blocked_reason,
                gm_connection,
            }) => {
                println!("    state: {state}");
                println!("    registered_at: {}", format_unix(registered_at));
                println!("    expires_at: {}", format_unix(expires_at));
                println!(
                    "    expires_in: {}",
                    format_expires_in(
                        expires_at,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    )
                );
                println!(
                    "    gm_connection: {}",
                    if gm_connection.is_empty() {
                        "unknown"
                    } else {
                        &gm_connection
                    }
                );
                match last_failure {
                    Some((t, msg)) => println!("    last_failure: {} {msg}", format_unix(Some(t))),
                    None => println!("    last_failure: none"),
                }
                println!("    can_answer: {can_answer}");
                if let Some(reason) = blocked_reason {
                    println!("    blocked_reason: {reason}");
                }
            }
            Ok(other) => {
                println!("    unexpected reply: {other:?}");
                line_ok = false;
            }
            Err(e) => {
                println!("    unreachable: {e}");
                line_ok = false;
            }
        }

        println!("  Recent calls (Agent B):");
        match query_status(&format!("{}:{}", line.veth_peer_addr, line.control_port)) {
            Ok(ControlMessage::CallHistoryReply { calls }) if calls.is_empty() => {
                println!("    (none)");
            }
            Ok(ControlMessage::CallHistoryReply { calls }) => {
                for c in calls {
                    println!(
                        "    {} caller={} outcome={} started={} ended={}",
                        c.call_id,
                        c.caller,
                        c.outcome,
                        format_unix(Some(c.started_at)),
                        format_unix(c.ended_at)
                    );
                }
            }
            Ok(other) => {
                println!("    unexpected reply: {other:?}");
                line_ok = false;
            }
            Err(e) => {
                println!("    unreachable: {e}");
                line_ok = false;
            }
        }

        any_ok = any_ok || line_ok;
    }

    // specs/027-discover-retry-health FR-006/FR-007: every configured
    // override that failed to become a line, printed after the resolved
    // ones — contracts/vowifi-status-output.md.
    for failed in resolution
        .failed
        .iter()
        .filter(|f| is_configured_line_failure(f))
    {
        println!(
            "Configured line {} (from config.toml): NOT RUNNING",
            failed.card_id
        );
        println!("  reason: {}", failed.reason);
    }

    if any_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub fn format_unix(t: Option<u64>) -> String {
    t.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string())
}

/// How long until the registration lapses, relative to `now`, rendered for an
/// operator rather than as a bare unix timestamp.
///
/// A lapsed binding is called out explicitly. During the 2026-08-16 outage the
/// status printed `expires_at: 1786878275` — a number nobody converts in their
/// head — directly above `can_answer: true`, and the disagreement went unnoticed
/// for nearly three hours.
pub fn format_expires_in(expires_at: Option<u64>, now: u64) -> String {
    match expires_at {
        None => "-".to_string(),
        Some(exp) => {
            let remaining =
                i64::try_from(exp).unwrap_or(i64::MAX) - i64::try_from(now).unwrap_or(0);
            if remaining < 0 {
                format!("{remaining}s (LAPSED)")
            } else {
                format!("{remaining}s")
            }
        }
    }
}

/// Connects to `addr` (`host:port`), sends `StatusQuery`, and returns
/// whatever single reply comes back. Used against both Agent A's status
/// port and Agent B's control port — each answers with the reply variant
/// it actually has data for (`RegistrationStatusReply` /
/// `CallHistoryReply` respectively).
pub fn query_status(addr: &str) -> BridgeResult<ControlMessage> {
    let socket_addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| BridgeError::Ims(format!("invalid address {addr}: {e}")))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_secs(3))
        .map_err(|e| BridgeError::Ims(format!("connect to {addr} failed: {e}")))?;
    write_msg(&mut stream, &ControlMessage::StatusQuery).map_err(BridgeError::Ims)?;
    let mut reader = std::io::BufReader::new(stream);
    read_msg(&mut reader).map_err(BridgeError::Ims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn expires_in_marks_a_lapsed_registration_rather_than_leaving_arithmetic_to_the_reader() {
        // The literal numbers from the 2026-08-16 outage.
        assert_eq!(
            format_expires_in(Some(1_786_878_275), 1_786_888_027),
            "-9752s (LAPSED)"
        );
    }

    #[test]
    fn expires_in_reports_remaining_time_while_the_registration_is_live() {
        assert_eq!(format_expires_in(Some(1_000_600), 1_000_000), "600s");
    }

    #[test]
    fn expires_in_is_blank_when_no_expiry_is_known() {
        assert_eq!(format_expires_in(None, 1_000_000), "-");
    }

    /// A `Read` that yields a scripted sequence of results, so a control
    /// message can be delivered in pieces with a `WouldBlock` (a poll-read
    /// timeout) in between — reproducing the specs/029 R7 fragmentation hazard
    /// deterministically, without a real socket.
    struct ChunkedReader {
        chunks: std::collections::VecDeque<std::io::Result<Vec<u8>>>,
    }

    impl std::io::Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.chunks.pop_front() {
                Some(Ok(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => Ok(0),
            }
        }
    }

    /// R7: a control message split across a poll-read timeout must be
    /// reassembled and parsed exactly once — a fresh buffer per read (as
    /// `read_msg` allocates) would drop the half that arrived first.
    #[test]
    fn poll_control_line_reassembles_a_message_split_across_a_poll_timeout() {
        let full = serde_json::to_string(&ControlMessage::CallRinging {
            call_id: "c1".to_string(),
        })
        .unwrap();
        let (a, b) = full.split_at(full.len() / 2);
        let mut chunks: std::collections::VecDeque<std::io::Result<Vec<u8>>> =
            std::collections::VecDeque::new();
        chunks.push_back(Ok(a.as_bytes().to_vec()));
        chunks.push_back(Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "poll timeout",
        )));
        chunks.push_back(Ok(format!("{b}\n").into_bytes()));
        let mut reader = std::io::BufReader::new(ChunkedReader { chunks });
        let mut pending = String::new();

        // First poll: partial data then a timeout — nothing complete yet, but
        // the partial line must survive for the next poll.
        assert!(matches!(
            poll_control_line(&mut reader, &mut pending),
            Ok(None)
        ));
        assert!(
            !pending.is_empty(),
            "the partial line must be retained across the timeout, not discarded"
        );

        // Second poll: the remainder arrives with its newline — one message,
        // parsed once, buffer cleared.
        match poll_control_line(&mut reader, &mut pending) {
            Ok(Some(ControlMessage::CallRinging { call_id })) => assert_eq!(call_id, "c1"),
            other => panic!("expected a single reassembled CallRinging, got {other:?}"),
        }
        assert!(
            pending.is_empty(),
            "the buffer must be cleared once a complete line is consumed"
        );
    }

    /// A cleanly closed connection (EOF) is a hard error, distinct from a
    /// mere poll timeout — the caller must stop, not spin.
    #[test]
    fn poll_control_line_reports_eof_as_error_not_timeout() {
        let mut reader = std::io::BufReader::new(ChunkedReader {
            chunks: std::collections::VecDeque::new(),
        });
        let mut pending = String::new();
        assert!(
            poll_control_line(&mut reader, &mut pending).is_err(),
            "a closed connection must surface as an error, not Ok(None)"
        );
    }

    /// specs/027-discover-retry-health review finding: this used to be keyed
    /// off `reason != "max_lines_exceeded"`, on the wrong assumption that
    /// `max_lines_exceeded` was the only reason an unpinned, auto-discovered
    /// candidate's failure could reach here — every other reason
    /// (`sim_unreadable`, `sim_locked`, `no_at_port`, ...) is just as
    /// reachable for one, and got mislabeled as a configured line from
    /// config.toml. Provenance (`FailedLine::configured`), not the reason
    /// string, is what actually distinguishes them.
    #[test]
    fn is_configured_line_failure_is_false_when_not_configured_regardless_of_reason() {
        for reason in [
            "not_found",
            "sim_absent",
            "sim_locked",
            "no_at_port",
            "sim_unreadable: CME 13",
            "max_lines_exceeded",
        ] {
            assert!(
                !is_configured_line_failure(&discovery::FailedLine::new("ec20-AAAAAA", reason)),
                "{reason} on an unpinned/auto-discovered candidate must not be \
                 reported as a configured-line failure"
            );
        }
    }

    /// The mirror case: a `configured: true` entry is always reported here
    /// regardless of reason — including `max_lines_exceeded`, since an
    /// operator who pins more lines than `max_lines` allows genuinely has a
    /// configured line that isn't running, which is exactly what this
    /// section exists to surface.
    #[test]
    fn is_configured_line_failure_is_true_when_configured_regardless_of_reason() {
        for reason in [
            "not_found",
            "sim_absent",
            "sim_locked",
            "no_at_port",
            "sim_unreadable: CME 13",
            "max_lines_exceeded",
        ] {
            assert!(
                is_configured_line_failure(
                    &discovery::FailedLine::new("ec20-AAAAAA", reason).configured(true)
                ),
                "{reason} on a configured/pinned candidate must be reported \
                 as a configured-line failure"
            );
        }
    }

    // A single test, not several — every case sets the same process-wide
    // GSM_SIP_BRIDGE_LINES_FILE env var `cargo test`'s parallel-within-
    // binary execution would otherwise race (matches the convention
    // `modules::discovery`'s own `excluded_ports_from_lines_file_behavior`
    // test already establishes for this exact env var).
    #[test]
    fn print_status_reports_a_configured_line_failure_even_with_zero_resolved_lines() {
        let dir = tempfile::tempdir().unwrap();

        // Before: zero lines, zero failures — the pre-existing "nothing to
        // report at all" case, unchanged.
        let empty_path = dir.path().join("empty.json");
        std::fs::write(&empty_path, r#"{"lines": [], "failed": []}"#).unwrap();
        std::env::set_var(crate::line::manifest::VOWIFI_LINES_ENV, &empty_path);
        assert_eq!(print_status(&VowifiConfig::default()), ExitCode::FAILURE);

        // specs/027-discover-retry-health: zero resolved lines but a
        // configured line failed to be discovered — must not hit the early
        // "no VoWiFi lines resolved, run discover first" bail before ever
        // reporting it (that used to be exactly what swallowed this case).
        let failed_path = dir.path().join("failed.json");
        std::fs::write(
            &failed_path,
            r#"{"lines": [], "failed": [{"card_id": "/dev/ttyUSB3", "reason": "not_found"}]}"#,
        )
        .unwrap();
        std::env::set_var(crate::line::manifest::VOWIFI_LINES_ENV, &failed_path);
        // Still FAILURE overall (no line actually answered), but it must
        // get there via the failed-line reporting path, not the early
        // bail — the exit code alone can't distinguish the two, so this
        // test's real value is that it doesn't panic/short-circuit before
        // reaching the new section (verified manually in quickstart.md's
        // "Configured line ... NOT RUNNING" output check).
        assert_eq!(print_status(&VowifiConfig::default()), ExitCode::FAILURE);
    }

    /// `read_line` documents that any bytes it already appended stay in
    /// the buffer even when it returns an error — but a fresh
    /// `String::new()` per `service_active_outbound_call` call used to
    /// throw that partial data away regardless, silently corrupting any
    /// message that happened to straddle a 200ms read-timeout boundary
    /// (specs/025-outbound-calling review). `pending_line` fixes this by
    /// persisting across calls; this proves the fix by forcing exactly
    /// that split.
    #[test]
    fn a_message_split_across_a_read_timeout_is_not_lost() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let mut sender = TcpStream::connect(addr).expect("connect");
        let (receiver, _) = listener.accept().expect("accept");
        receiver
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set_read_timeout");

        let mut ac = ActiveOutboundCall {
            call: Call::from_id(0, CallState::Confirmed),
            veth_call: Call::from_id(1, CallState::Confirmed),
            call_id: "out-test".to_string(),
            control: std::io::BufReader::new(receiver),
            pending_line: String::new(),
        };

        let full = serde_json::to_string(&ControlMessage::CallEnded {
            call_id: "out-test".to_string(),
            reason: "caller_hangup".to_string(),
        })
        .expect("serialize")
            + "\n";
        let full_bytes = full.as_bytes();
        let split_at = full_bytes.len() / 2;
        sender
            .write_all(&full_bytes[..split_at])
            .expect("write first half");

        // Only half the message has arrived: `read_line` must time out
        // mid-line. `call` is `Confirmed`, not `Disconnected`, so this
        // must not be mistaken for the phone leg hanging up either.
        assert!(
            !service_active_outbound_call(&mut ac),
            "must not end the call on a mid-message timeout"
        );

        std::thread::sleep(Duration::from_millis(100));
        sender
            .write_all(&full_bytes[split_at..])
            .expect("write second half");

        // The rest arrives: if the first half had been discarded, this
        // would parse as a malformed fragment (or the wrong message) and
        // return `false` instead of correctly recognizing `CallEnded`.
        assert!(
            service_active_outbound_call(&mut ac),
            "the reassembled message must parse as CallEnded"
        );
    }

    fn record(id: &str) -> CallRecord {
        CallRecord {
            call_id: id.to_string(),
            caller: "+919000000000".to_string(),
            outcome: "answered:caller_hangup".to_string(),
            started_at: 1_700_000_000,
            ended_at: Some(1_700_000_300),
        }
    }

    #[test]
    fn carrier_status_from_reason_reads_the_leading_sip_code() {
        for (reason, want) in [
            ("486 Busy Here", Some(486)),
            ("480 Temporarily Unavailable", Some(480)),
            ("503 Service Unavailable", Some(503)),
            ("no final response from carrier: timed out", None),
            ("bad SDP answer: parse error", None),
            ("", None),
            // Not exactly 3 digits — not a plausible SIP status.
            ("42 not a status", None),
            ("4860 also not a status", None),
        ] {
            assert_eq!(carrier_status_from_reason(reason), want, "{reason:?}");
        }
    }

    #[test]
    fn committed_failure_outcome_distinguishes_unanswered_from_refused() {
        for (reason, want_code, want_outcome) in [
            (
                format!(
                    "{}: no final response from carrier: timed out",
                    control::reason::CARRIER_TIMEOUT
                ),
                480,
                OutboundAttemptOutcome::Unanswered,
            ),
            (
                "480 Temporarily Unavailable".to_string(),
                480,
                OutboundAttemptOutcome::Unanswered,
            ),
            (
                "486 Busy Here".to_string(),
                486,
                OutboundAttemptOutcome::RefusedNetworkFailure,
            ),
            (
                "bad SDP answer: parse error".to_string(),
                503,
                OutboundAttemptOutcome::RefusedNetworkFailure,
            ),
        ] {
            let (code, outcome) = outbound_outcome_for_committed_failure(&reason);
            assert_eq!(code, want_code, "{reason:?}");
            assert_eq!(outcome, want_outcome, "{reason:?}");
        }
    }

    #[test]
    fn committed_failure_outcome_clamps_non_failure_codes_to_503() {
        for reason in [
            // A redirect: answering with it verbatim would need a Contact
            // header this call site can't supply.
            "302 Moved Temporarily",
            // A 2xx landing in a *failure* reason string is not something
            // `Call::answer` should ever pass through as-is.
            "202 Accepted",
            // A 1xx provisional has no business here either.
            "183 Session Progress",
        ] {
            let (code, outcome) = outbound_outcome_for_committed_failure(reason);
            assert_eq!(code, 503, "{reason:?}");
            assert_eq!(
                outcome,
                OutboundAttemptOutcome::RefusedNetworkFailure,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn recent_calls_evicts_oldest_once_over_capacity() {
        let mut recent = RecentCalls::new(3);
        recent.push(record("1"));
        recent.push(record("2"));
        recent.push(record("3"));
        recent.push(record("4"));
        let snapshot = recent.snapshot();
        assert_eq!(snapshot.len(), 3);
        // Newest first; "1" was evicted.
        assert_eq!(snapshot[0].call_id, "4");
        assert_eq!(snapshot[1].call_id, "3");
        assert_eq!(snapshot[2].call_id, "2");
    }

    #[test]
    fn recent_calls_under_capacity_keeps_everything() {
        let mut recent = RecentCalls::new(5);
        recent.push(record("1"));
        recent.push(record("2"));
        let snapshot = recent.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].call_id, "2");
        assert_eq!(snapshot[1].call_id, "1");
    }

    #[test]
    fn recent_calls_empty_snapshot_when_nothing_pushed() {
        let recent = RecentCalls::new(5);
        assert!(recent.snapshot().is_empty());
    }

    #[test]
    fn control_bind_retries_until_the_veth_appears() {
        // Regression test for a live two-line failure: the veth an Agent B
        // listener binds to is created only once that line's tunnel is up, so
        // the bind legitimately fails for a while. It used to be one-shot, and
        // the slower line was then permanently unable to receive calls.
        let mut attempts = 0;
        let mut slept = Vec::new();
        let bound = bind_with_retry(
            "ec20-51212",
            "10.99.0.2:7050",
            Duration::from_millis(1),
            || {
                attempts += 1;
                if attempts < 4 {
                    Err("Address not available (os error 99)")
                } else {
                    Ok("listener")
                }
            },
            |d| slept.push(d),
        );
        assert_eq!(bound, "listener");
        assert_eq!(attempts, 4, "must keep retrying, not give up on the first");
        assert_eq!(slept.len(), 3, "one sleep between each failed attempt");
    }

    #[test]
    fn control_bind_does_not_sleep_when_the_address_is_already_available() {
        let mut slept = Vec::new();
        let bound = bind_with_retry(
            "pcsc0",
            "10.99.0.6:7050",
            Duration::from_secs(2),
            || Ok::<_, String>("listener"),
            |d| slept.push(d),
        );
        assert_eq!(bound, "listener");
        assert!(slept.is_empty(), "a first-try bind must not delay startup");
    }

    /// Found live (specs/025-outbound-calling T072): with `CALL_ATTEMPT_TIMEOUT`
    /// no longer than Agent A's own carrier-INVITE wait, a real, ringing
    /// call gets abandoned mid-flight — Agent B moves on to the next line
    /// (or gives up) while the carrier is still working on the current one,
    /// and the carrier can go on to answer a call nobody is listening for.
    /// This can't check the two constants are literally equal-or-related at
    /// compile time (Agent A runs as a separate OS process, even though
    /// it's the same compiled binary), so it asserts the relationship here
    /// instead, with room for the veth handoff on top of Agent A's own wait.
    #[test]
    fn call_attempt_timeout_exceeds_agent_as_invite_wait() {
        let agent_a_max_wait =
            crate::ims::agent::OUTBOUND_INVITE_TIMEOUT + crate::ims::agent::OUTBOUND_RING_TIMEOUT;
        assert!(
            CALL_ATTEMPT_TIMEOUT > agent_a_max_wait,
            "CALL_ATTEMPT_TIMEOUT ({CALL_ATTEMPT_TIMEOUT:?}) must exceed \
             ims::agent::OUTBOUND_INVITE_TIMEOUT + OUTBOUND_RING_TIMEOUT \
             ({agent_a_max_wait:?}) with margin for the veth handoff, or a \
             real carrier call can outlive Agent B's patience",
        );
    }

    /// Found live (specs/025-outbound-calling T072): Agent A's dispatch loop
    /// only re-checks its `place_call_rx` channel once per iteration, and
    /// its one blocking wait is bounded by `IDLE_POLL_INTERVAL` — so a
    /// `PlaceCall` can sit unnoticed for up to that long before Agent A
    /// even sends `CallAttempting`. `PLACE_CALL_TIMEOUT` must stay bigger,
    /// with real margin for the connect/write/read round trip on top, or
    /// this line gets marked unavailable before Agent A ever gets a chance
    /// to answer.
    #[test]
    fn place_call_timeout_exceeds_agent_as_idle_poll() {
        assert!(
            PLACE_CALL_TIMEOUT > crate::ims::agent::IDLE_POLL_INTERVAL * 2,
            "PLACE_CALL_TIMEOUT ({PLACE_CALL_TIMEOUT:?}) must clear \
             ims::agent::IDLE_POLL_INTERVAL ({:?}) with real margin, or a \
             PlaceCall can time out before Agent A's dispatch loop even \
             notices it",
            crate::ims::agent::IDLE_POLL_INTERVAL,
        );
    }
}
