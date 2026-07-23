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
use crate::control::protocol::{AgentKind, AgentState, ObservedEvent, SmsOutcome};
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
/// one config file and, in the merged deployment (`docker/entrypoint.sh`),
/// one network namespace (host networking) — reusing `[sip].local_port` for
/// both means two independent `pjsua_create`/transport-bind calls racing for
/// the same UDP port, which fails outright for whichever one starts second.
/// Same "private implementation detail" status as `VETH_SIP_PORT`/
/// `AGENT_A_STATUS_PORT`.
pub const AGENT_B_SIP_LOCAL_PORT: u16 = 5072;

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
}

/// Reads the `discover` subcommand's line-resolution file and returns every
/// resolved VoWiFi line. Falls back to a single legacy line built straight
/// from `config` (today's pre-multi-card behavior, `LEGACY_LINE_CARD_ID` as
/// its card id) when the file is missing/empty — the common case for an
/// existing single-SIM deployment that has never run `discover` (FR-020).
fn resolve_runtime_lines(config: &VowifiConfig) -> Vec<RuntimeLine> {
    let path = lines_file_path();
    match discovery::read_line_resolution(&path) {
        Ok(resolution) if !resolution.lines.is_empty() => resolution
            .lines
            .iter()
            .map(|l| RuntimeLine {
                index: l.index,
                card_id: l.card_id.clone(),
                veth_local_addr: l.veth_local_addr.clone(),
                veth_peer_addr: l.veth_peer_addr.clone(),
                control_port: l.control_port,
                sip_leg_port: VETH_SIP_PORT,
            })
            .collect(),
        _ => vec![RuntimeLine {
            index: 0,
            card_id: LEGACY_LINE_CARD_ID.to_string(),
            veth_local_addr: config.veth_local_addr.clone(),
            veth_peer_addr: config.veth_peer_addr.clone(),
            control_port: config.control_port,
            sip_leg_port: VETH_SIP_PORT,
        }],
    }
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
    let lines = resolve_runtime_lines(&config.vowifi);
    run_telephony_side(
        config,
        AGENT_B_SIP_LOCAL_PORT,
        config.vowifi.wideband,
        lines,
        "vowifi-sip-agent",
        crate::store::Transport::Vowifi,
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
    if wideband {
        prioritize_wideband_codecs(&endpoint);
    }

    let acc_config = AccountConfig {
        sip_server: config.sip.server.clone(),
        sip_port: config.sip.port,
        username: config.sip.username.clone(),
        password: config.sip.password.expose_secret().clone(),
        display_name: config.sip.display_name.clone(),
    };
    let account = Account::register(&endpoint, acc_config, None)
        .map_err(|e| BridgeError::Ims(format!("SIP account registration failed: {e}")))?;
    tracing::info!(
        server = %config.sip.server,
        port = config.sip.port,
        agent = agent_label,
        "registered to PBX"
    );

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

    // One accept-loop thread per line, all sharing the one endpoint/account/
    // Discord client/store/runtime above — `std::thread::scope` blocks until
    // every thread finishes, which in practice is never (each loops forever
    // like the pre-multi-card single loop did), so this call never returns
    // in normal operation, matching today's behavior for the N=1 case.
    std::thread::scope(|scope| {
        for line in &lines {
            let endpoint = &endpoint;
            let account = &account;
            let recent_calls = Arc::clone(&recent_calls);
            let discord_client = &discord_client;
            let sms_runtime = &sms_runtime;
            let store_tx = store.sender();
            let card_id = line.card_id.clone();
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
                    config,
                    &recent_calls,
                    discord_client,
                    sms_runtime,
                    store_tx,
                );
            });
        }
    });
    Ok(())
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
#[allow(clippy::too_many_arguments)]
fn run_line_listener(
    listen_addr: (String, u16),
    card_id: &str,
    leg_addr: &str,
    leg_port: u16,
    transport: crate::store::Transport,
    endpoint: &Endpoint,
    account: &Account,
    config: &AppConfig,
    recent_calls: &Arc<Mutex<HashMap<String, RecentCalls>>>,
    discord_client: &Option<DiscordClient>,
    sms_runtime: &Runtime,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
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

    let listener = match TcpListener::bind((listen_addr.0.as_str(), listen_addr.1)) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(
                card_id = %card_id,
                addr = %format!("{}:{}", listen_addr.0, listen_addr.1),
                error = %e,
                "control channel listen failed for this line; it will not receive calls"
            );
            return;
        }
    };
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
            config,
            recent_calls,
            discord_client,
            sms_runtime,
            store_tx.clone(),
            &reporter,
        ) {
            tracing::warn!(card_id = %card_id, error = %e, "error handling Agent A control connection");
        }
    }
}

/// Fallback card id used only when no `discover`-produced line resolution
/// exists (`resolve_runtime_lines`'s legacy single-line branch, and
/// `main.rs`'s `vowifi-ims-agent` with no `--line`) — the pre-multi-card
/// label, kept as the default so an unresolved deployment's Discord/log/
/// metrics attribution doesn't change (FR-020). A resolved multi-line
/// deployment uses each line's real card id instead (FR-017).
pub const LEGACY_LINE_CARD_ID: &str = "vowifi";

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
    match DiscordClient::new(config.sms.discord_webhook_url.clone()) {
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
    config: &AppConfig,
    recent_calls: &Arc<Mutex<HashMap<String, RecentCalls>>>,
    discord_client: &Option<DiscordClient>,
    sms_runtime: &Runtime,
    store_tx: crossbeam_channel::Sender<StoreCommand>,
    reporter: &Reporter,
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

    match bridge_call(endpoint, account, config, &caller, leg_addr, leg_port) {
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
fn bridge_call(
    endpoint: &Endpoint,
    account: &Account,
    config: &AppConfig,
    caller: &str,
    leg_addr: &str,
    leg_port: u16,
) -> BridgeResult<(Call, Call)> {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let pai_value;
    if !caller.is_empty() {
        pai_value = format!("\"{caller}\" <tel:{caller}>");
        headers.push(("P-Asserted-Identity", &pai_value));
        headers.push(("X-GSM-Caller-ID", caller));
    }

    let pbx_uri = pbx_dest_uri(config, caller);
    let pbx_call = Call::make(account, &pbx_uri, None, &headers)
        .map_err(|e| BridgeError::Ims(format!("PBX-side call failed: {e}")))?;

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

/// Mirrors `crate::sip::SipBridge::compute_destination_uri`: empty
/// `[bridge].sip_destination` means DID passthrough (dial the caller's own
/// number at the PBX), otherwise dial the configured fixed extension.
fn pbx_dest_uri(config: &AppConfig, caller_did: &str) -> String {
    let raw_dest = if config.bridge.sip_destination.is_empty() {
        caller_did
    } else {
        &config.bridge.sip_destination
    };
    let dest = raw_dest.trim_start_matches('+');
    format!("sip:{dest}@{}:{}", config.sip.server, config.sip.port)
}

/// Entry point for the `vowifi-status` subcommand: queries every resolved
/// line's Agent A registration health (`AGENT_A_STATUS_PORT`, reached via
/// that line's own veth-local address) and Agent B's per-line recent call
/// history (that line's own veth-peer address/control port), printing one
/// labeled block per line — FR-018/User Story 3. A query failing for one
/// line is reported for that line only, not fatal to reporting the others
/// (acceptance scenario 1); overall failure means *every* line's queries
/// failed.
pub fn print_status(config: &VowifiConfig) -> ExitCode {
    let lines = resolve_runtime_lines(config);
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
            }) => {
                println!("    state: {state}");
                println!("    registered_at: {}", format_unix(registered_at));
                println!("    expires_at: {}", format_unix(expires_at));
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

    if any_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub fn format_unix(t: Option<u64>) -> String {
    t.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string())
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

    fn record(id: &str) -> CallRecord {
        CallRecord {
            call_id: id.to_string(),
            caller: "+919789063708".to_string(),
            outcome: "answered:caller_hangup".to_string(),
            started_at: 1_700_000_000,
            ended_at: Some(1_700_000_300),
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
}
