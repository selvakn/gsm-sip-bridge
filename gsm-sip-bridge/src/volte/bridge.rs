//! Bridging inbound cellular calls over the host-side LTE registration
//! (specs/017-volte-inbound-bridge, US1/US2).
//!
//! # One process for Agent B, one process per line for Agent A
//!
//! The Wi-Fi path splits into Agent A (carrier side) and Agent B (telephone
//! side) because the ePDG tunnel puts them in different network namespaces and
//! PJSIP cannot cross that boundary. Feature 017 originally judged that this
//! split "buys nothing" for LTE, since the LTE path had no namespace of its
//! own (specs/015 research R4) — true only as long as there was ever one
//! interface to bridge. specs/020-volte-line-netns revisits exactly that,
//! once multi-line VoLTE (specs/018) made it not true any more: each line now
//! gets its own namespace, for the same reason VoWiFi's lines do (a shared
//! routing table cannot otherwise guarantee a line's traffic uses its own
//! interface — see that feature's research.md).
//!
//! What stays true is the reuse: `ims::agent`'s INVITE handling, ringback,
//! RTP relay and hangup propagation are the most carefully-tuned code in the
//! tree, and FR-019/SC-008 require one implementation serving both paths. So
//! this module still reuses that logic verbatim — [`super::carrier_agent`]
//! now holds the per-line body, callable either as a thread here (this
//! process, no namespace — the single-`--modem` diagnostic path only,
//! research.md R7) or as its own `volte-carrier-agent` process
//! (`docker/entrypoint.sh`, inside that line's namespace, the production
//! path). This module (`bridge::run`) now runs only the shared telephone-side
//! half (Agent B) for the production path — see [`ServiceConfig::spawn_carrier_threads`].
//!
//! | Wi-Fi path | Diagnostic (`--modem`) | Production (auto-discovered) |
//! |---|---|---|
//! | Two processes | One process, two threads | Two-plus processes |
//! | veth pair | loopback | veth pair (specs/020-volte-line-netns) |
//! | Agent B's own SIP port | a **third** local port ([`SIP_LOCAL_PORT`]) | same |
//!
//! The control protocol is unchanged either way — only the address
//! `run_telephony_side` reaches each line's carrier half at differs (loopback
//! vs. a real veth address).
//!
//! # Why a third port
//!
//! The codebase already carries a scar from two endpoints racing for one
//! (`vowifi::AGENT_B_SIP_LOCAL_PORT`): reusing `[sip].local_port` for both
//! means two `pjsua_create`/transport-bind calls racing for the same UDP port,
//! which fails outright for whichever starts second. This service runs
//! alongside the circuit-switched daemon in the same container and network
//! namespace, so it needs its own (research R3).
//!
//! # Maintenance must yield to a call
//!
//! Renewal deferral is inherited from the Wi-Fi agent. **Re-attachment
//! deferral is new** and is the hazard this feature actually adds: the carrier
//! tears the LTE attachment down roughly every two hours (specs/015 research
//! R15) and the registration loop re-attaches automatically. Unguarded, that
//! would drop a live call roughly every two hours. See
//! [`crate::ims::lifecycle::MaintenancePolicy`].

use crate::config::AppConfig;
use crate::error::{BridgeError, BridgeResult};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::time::Duration;

use super::VolteSettings;

/// This service's own telephone-side local port.
///
/// Deliberately distinct from `[sip].local_port` (the circuit-switched daemon)
/// and `vowifi::AGENT_B_SIP_LOCAL_PORT` (5072). Three endpoints can now live
/// in one network namespace without racing for a bind (FR-021, research R3).
pub const SIP_LOCAL_PORT: u16 = 5073;

/// Loopback SIP port where the carrier-side half listens for the
/// telephone-side half's leg — the veth link's replacement.
pub const LOOPBACK_SIP_PORT: u16 = 5074;

/// Loopback control port joining the two halves. Same protocol the Wi-Fi path
/// uses, same message shapes.
pub const LOOPBACK_CONTROL_PORT: u16 = 5075;

/// Loopback registration-status port the carrier half's status listener binds
/// (the `volte-status`/`print_live_status` query target). Distinct from
/// `vowifi::AGENT_A_STATUS_PORT` (5071): with several LTE lines sharing one
/// network namespace (specs/018-volte-multi-modem), each line's carrier half
/// needs its own status port, so this is the line-0 base and
/// `volte::discovery` derives the rest per line.
pub const LOOPBACK_STATUS_PORT: u16 = 5076;

/// Card label used when none is supplied — the single-line case.
pub const DEFAULT_CARD_ID: &str = "volte";

/// Loopback — used whenever a line has no veth link (the single-`--modem`
/// diagnostic path, research.md R7), where both halves stay threads in this
/// one process and neither leg ever leaves the host. `pub(super)` so
/// `carrier_agent::run` (the other caller of the same fallback) can reuse it
/// instead of duplicating the constant.
pub(super) const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// One host-side LTE line ready to run: a modem's attachment settings, its
/// PBX-facing identity, and its own loopback port trio. The multi-modem unit
/// (specs/018-volte-multi-modem) — the single-line service is just `lines`
/// of length one.
pub struct BridgeLine {
    /// Labels this line's metrics and call history.
    pub card_id: String,
    /// The LTE attachment this line's registration rides on — including its
    /// own `restore_cid_path` so each modem's displaced context is restored
    /// independently.
    pub settings: VolteSettings,
    pub msisdn: Option<String>,
    /// This line's carrier↔telephony leg port, control port, and status port —
    /// derived per line by [`super::discovery`] so several lines share one
    /// namespace without racing for a bind.
    pub sip_leg_port: u16,
    pub control_port: u16,
    pub status_port: u16,
    /// This line's network namespace (specs/020-volte-line-netns). Empty for
    /// the single-`--modem` diagnostic path (no namespace exists); informational
    /// only here — the manifest carries it for `docker/entrypoint.sh`'s cleanup
    /// (research.md R6), nothing in this process itself joins a namespace.
    pub netns: String,
    /// This line's carrier-agent-side veth address. Empty means "no netns for
    /// this line" — [`carrier_agent::run`] then binds `LOOPBACK` instead,
    /// exactly as before this feature (research.md R2).
    pub veth_carrier_addr: String,
    /// This line's telephony-half-side veth address, for the same reason.
    pub veth_telephony_addr: String,
}

/// Everything the service needs to start.
pub struct ServiceConfig {
    /// The lines to bridge — one per modem. Never empty (the caller fails
    /// before constructing this if nothing resolved).
    pub lines: Vec<BridgeLine>,
    /// Proceed even if the Wi-Fi path appears to hold the same subscriber's
    /// registration. An escape hatch for a stale detection, not a default.
    pub force: bool,
    /// Whether this process should also run each line's carrier half as an
    /// in-process thread (specs/020-volte-line-netns).
    ///
    /// **`true` only for the single-`--modem` diagnostic invocation** — a
    /// manual, operator-run test with no `docker/entrypoint.sh` orchestration
    /// and so no namespace for a line to be isolated into; this reproduces
    /// exactly what this process did before this feature.
    ///
    /// **`false` for the production, auto-discovered path**: each line's
    /// carrier half instead runs as its own `volte-carrier-agent --line N`
    /// process, launched by `docker/entrypoint.sh` inside that line's own
    /// namespace (research.md R3) — this process runs only the shared
    /// telephony half (Agent B) and must not *also* run the carrier halves
    /// in-process, which would answer every call twice.
    pub spawn_carrier_threads: bool,
}

/// Entry point for the host-side cellular bridging service.
pub fn run(service: ServiceConfig, app_config: &AppConfig) -> ExitCode {
    match run_inner(service, app_config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(service: ServiceConfig, app_config: &AppConfig) -> BridgeResult<()> {
    // Both paths register the *same* subscriber, with the same IMPU and the
    // same IMEI-derived instance id. Two live registrations would have the
    // network deliver calls to whichever bound last, silently — so refuse to
    // start rather than produce an outage that looks like a carrier fault
    // (FR-022). One check covers the whole container: VoWiFi and VoLTE stay
    // mutually exclusive at the container level even when VoLTE runs several
    // lines.
    super::guard::check_no_vowifi_conflict(service.force).map_err(BridgeError::Ims)?;

    let lines = service.lines;
    tracing::info!(
        line_count = lines.len(),
        lines = ?lines.iter().map(|l| l.card_id.clone()).collect::<Vec<_>>(),
        "resolved host-side LTE lines"
    );

    // Persist the line manifest so `docker/entrypoint.sh`'s cleanup can tear
    // down every line's PDN and `volte-status` can find every line's ports —
    // but only for the diagnostic single-`--modem` path, which owns and is
    // the sole writer of its own manifest. The production, auto-discovered
    // path's manifest is already there (`volte-discover-lines` wrote it
    // before this process started, research.md R7) and MUST NOT be
    // overwritten here: this process only resolves loopback fallbacks for
    // veth addresses, never the P-CSCF override or restore-cid path a
    // `volte-carrier-agent` process still needs to read correctly on its own
    // later restarts.
    if service.spawn_carrier_threads {
        write_manifest(&lines);
    }

    // The telephone-system half is shared across every line — one PJSIP
    // endpoint, one PBX registration, one accept-loop thread per line. This is
    // the exact same code the Wi-Fi path runs (FR-019); only the ports (and,
    // as of specs/020-volte-line-netns, the veth addresses replacing loopback
    // for the netns-isolated production path) differ. An empty
    // `veth_carrier_addr`/`veth_telephony_addr` (the diagnostic single-`--modem`
    // path — research.md R7) falls back to `LOOPBACK`, exactly as before this
    // feature.
    let telephony_lines: Vec<crate::vowifi::RuntimeLine> = lines
        .iter()
        .map(|l| crate::vowifi::RuntimeLine {
            index: l.settings_index(),
            card_id: l.card_id.clone(),
            veth_local_addr: non_empty_or_loopback(&l.veth_carrier_addr),
            veth_peer_addr: non_empty_or_loopback(&l.veth_telephony_addr),
            control_port: l.control_port,
            sip_leg_port: l.sip_leg_port,
        })
        .collect();

    // Whether the telephone-side half has the PBX registration the outbound
    // bridge leg needs. All halves are threads here, so they share it directly:
    // the telephony half sets it, every carrier half reads it for admission and
    // status (a call cannot be bridged if the PBX leg is unregistered).
    let pbx_registered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Everything runs inside one scope: the shared telephony half, and per line
    // a modem SMS reader plus the carrier half. `serve_inbound`/
    // `run_telephony_side` never return in normal operation, so the scope
    // blocks forever, exactly as the single-line service's `serve_inbound` call
    // did. A per-line failure (attach, registration) is logged and ends only
    // that line's threads; the others keep running.
    std::thread::scope(|scope| {
        {
            let pbx_registered = pbx_registered.clone();
            let telephony_lines = telephony_lines.clone();
            std::thread::Builder::new()
                .name("volte-telephony".into())
                .spawn_scoped(scope, move || {
                    // Retry like the carrier halves: a transient bind failure at
                    // startup (e.g. EADDRINUSE while a prior container releases the
                    // port) must not leave the shared PBX leg down for good — that
                    // would strand every line's calls even though registration is
                    // up. Clear the shared flag while it is down so carriers
                    // fast-decline and status reads `can_answer=false`.
                    loop {
                        if let Err(e) = crate::vowifi::run_telephony_side(
                            app_config,
                            SIP_LOCAL_PORT,
                            true,
                            telephony_lines.clone(),
                            "volte-bridge",
                            crate::store::Transport::Volte,
                            Some(pbx_registered.clone()),
                        ) {
                            tracing::error!(
                                error = %e,
                                retry_in_secs = LINE_RETRY_BACKOFF.as_secs(),
                                "the telephone-side half stopped; retrying"
                            );
                        }
                        pbx_registered.store(false, std::sync::atomic::Ordering::SeqCst);
                        std::thread::sleep(LINE_RETRY_BACKOFF);
                    }
                })
                .expect("failed to start the telephone side");
        }

        // Give the telephone-side half a moment to bind every line's control
        // port before any carrier side offers a call. A call arriving in this
        // window would otherwise fail its control connection and be declined —
        // rare, but it costs nothing to close.
        std::thread::sleep(TELEPHONY_STARTUP_GRACE);

        // Production, auto-discovered lines run their carrier half as a
        // separate, netns-isolated `volte-carrier-agent` process launched by
        // `docker/entrypoint.sh` (research.md R3) — spawning it *again* here,
        // in-process, would answer every call twice. Only the single-`--modem`
        // diagnostic invocation (no entrypoint orchestration, nothing to
        // isolate into) still runs its one line's carrier half as a thread of
        // this process, exactly as every line did before this feature.
        if service.spawn_carrier_threads {
            for line in lines {
                let pbx_registered = pbx_registered.clone();
                std::thread::Builder::new()
                    .name(format!("volte-line-{}", line.card_id))
                    .spawn_scoped(scope, move || {
                        run_line(&line, app_config, pbx_registered);
                    })
                    .expect("failed to start a carrier line");
            }
        }
    });

    Ok(())
}

/// `LOOPBACK` when `addr` is empty (research.md R7 — the diagnostic
/// single-`--modem` path has no veth link), else `addr` itself.
fn non_empty_or_loopback(addr: &str) -> String {
    if addr.is_empty() {
        LOOPBACK.to_string()
    } else {
        addr.to_string()
    }
}

/// Runs one line for the life of the process: its modem SMS reader once, then
/// its carrier half ([`carrier_agent::run`]) in a retry loop. **Only used for
/// the single-`--modem` diagnostic path** (`spawn_carrier_threads`,
/// research.md R7) — the production, auto-discovered path runs each line's
/// carrier half as its own `volte-carrier-agent` process instead, supervised
/// (restarted on exit) by `docker/entrypoint.sh`, mirroring how
/// `vowifi-ims-agent` is supervised rather than retrying internally.
fn run_line(
    line: &BridgeLine,
    app_config: &AppConfig,
    pbx_registered: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    // One lock guards every touch of this modem's AT port so two users never
    // interleave on it (research R6): the carrier half (attach, registration,
    // renewal, re-attachment) and the modem SMS reader. Each line has its own
    // modem, so each has its own lock. Both outlive every retry below.
    let modem_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    let control_addr = SocketAddr::new(LOOPBACK, line.control_port);

    // The circuit-switched SMS route (FR-036): the carrier may deliver a text
    // into the modem's own storage rather than as an IMS `MESSAGE`, and with
    // this card assigned exclusively here, nothing else reads that storage.
    // Spawned once (its own reader loop is resilient) and serialised with the
    // carrier half through the shared lock; it relays each message onto the
    // same telephone-side recorder the IMS route uses.
    {
        let modem_port = line.settings.modem_port.clone();
        let lock = modem_lock.clone();
        if let Err(e) = std::thread::Builder::new()
            .name(format!("volte-sms-{}", line.card_id))
            .spawn(move || super::sms::run_modem_reader(modem_port, control_addr, lock))
        {
            tracing::error!(card_id = %line.card_id, error = %e, "failed to start the modem SMS reader for this line");
        }
    }

    loop {
        super::carrier_agent::run(
            line,
            app_config,
            modem_lock.clone(),
            Some(pbx_registered.clone()),
        );
        // The carrier half returned — a failed attach/registration or a lost
        // one. Back off before retrying, so a persistent fault (no SIM, no
        // coverage) does not spin on the modem or the registrar. `pbx_registered`
        // is NOT touched here: it names the shared telephone-side half's PBX
        // trunk registration, not this line's carrier state, and every line
        // shares one `Arc` — clearing it on a single line's failure would
        // falsely mark every *other* healthy line unable to answer too (found
        // live-testing: one line's modem stuck retrying stomped a working
        // sibling line's `can_answer` to false). Only `run_inner`'s telephony
        // retry loop owns writes to this flag.
        tracing::warn!(
            card_id = %line.card_id,
            retry_in_secs = LINE_RETRY_BACKOFF.as_secs(),
            "carrier half for this line stopped; retrying"
        );
        std::thread::sleep(LINE_RETRY_BACKOFF);
    }
}

/// How long a line waits before retrying its carrier half after a failure.
/// Deliberately unhurried: a retry re-runs PDN attachment and a full IMS-AKA
/// exchange, so a tight loop would hammer both the modem and the registrar —
/// the same reasoning as `docker/entrypoint.sh`'s 15s LTE restart backoff.
const LINE_RETRY_BACKOFF: Duration = Duration::from_secs(15);

impl BridgeLine {
    /// Line index recovered from its status port — used only to label the
    /// telephony `RuntimeLine`, which never relies on it for addressing.
    fn settings_index(&self) -> u32 {
        ((self.status_port - LOOPBACK_STATUS_PORT) / super::discovery::LINE_PORT_STRIDE) as u32
    }
}

/// Writes the line manifest so cleanup and `volte-status` agree on what is
/// running. Best-effort: a failure here degrades status/cleanup but must not
/// stop the service from carrying calls.
fn write_manifest(lines: &[BridgeLine]) {
    use super::discovery::{VolteLineManifest, VolteLineManifestEntry};
    let manifest = VolteLineManifest {
        lines: lines
            .iter()
            .map(|l| VolteLineManifestEntry {
                index: l.settings_index(),
                card_id: l.card_id.clone(),
                modem_port: l.settings.modem_port.to_string_lossy().to_string(),
                cid: l.settings.cid,
                apn: l.settings.apn.clone(),
                iface: l.settings.iface.clone(),
                restore_cid_path: l
                    .settings
                    .restore_cid_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                status_port: l.status_port,
                control_port: l.control_port,
                sip_leg_port: l.sip_leg_port,
                netns: l.netns.clone(),
                veth_carrier_addr: l.veth_carrier_addr.clone(),
                veth_telephony_addr: l.veth_telephony_addr.clone(),
                pcscf: l.settings.pcscf.map(|a| a.to_string()).unwrap_or_default(),
                msisdn: l.msisdn.clone().unwrap_or_default(),
            })
            .collect(),
    };
    let path = super::discovery::manifest_path();
    match serde_json::to_string_pretty(&manifest) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(path = %path.display(), error = %e, "could not write the VoLTE line manifest; cleanup and status may be incomplete");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not serialize the VoLTE line manifest"),
    }
}

/// How long to let the telephone-side half bind before answering calls.
const TELEPHONY_STARTUP_GRACE: Duration = Duration::from_millis(500);

/// Queries the **running** service for its live state and prints it, returning
/// `true` when the service answered (US3, FR-033).
///
/// This is the status source that matters while the service is up, and it must
/// be tried before reading the modem: the service owns the modem's AT port
/// exclusively (research R6), so a second reader probing `AT+CGACT?` on it
/// races the service mid-transaction — the documented "no status in response"
/// hazard. When the service answers here, the caller must **not** fall back to
/// touching the modem.
///
/// Both halves run as loopback threads. With several lines
/// (specs/018-volte-multi-modem) each carrier half's registration listener
/// and each telephone-side accept loop sit on that line's own derived ports,
/// read from the line manifest the running bridge wrote. A single line is
/// just the one-entry case; a missing manifest falls back to line 0's default
/// ports so a legacy single-line service is still reachable.
///
/// Returns `false` only when **no** line's registration half is reachable,
/// which is what tells the caller the service is not running and a direct
/// modem read is safe.
pub fn print_live_status() -> bool {
    // (card_id, status_port, control_port, carrier_addr, telephony_addr) per
    // line — from the manifest, or the line-0 loopback defaults when it is
    // absent. `carrier_addr`/`telephony_addr` empty means `LOOPBACK`
    // (research.md R7: no netns for this line — same reachability the status
    // query has always had). A real veth address is reachable directly from
    // the default namespace `volte-status` itself runs in, the same way
    // Agent B already reaches VoWiFi's Agent A across that link.
    let lines: Vec<(String, u16, u16, String, String)> =
        match super::discovery::read_manifest(&super::discovery::manifest_path()) {
            Ok(m) if !m.lines.is_empty() => m
                .lines
                .iter()
                .map(|l| {
                    (
                        l.card_id.clone(),
                        l.status_port,
                        l.control_port,
                        l.veth_carrier_addr.clone(),
                        l.veth_telephony_addr.clone(),
                    )
                })
                .collect(),
            _ => vec![(
                DEFAULT_CARD_ID.to_string(),
                LOOPBACK_STATUS_PORT,
                LOOPBACK_CONTROL_PORT,
                String::new(),
                String::new(),
            )],
        };

    println!("Live service (querying the running bridge, not the modem):");
    let mut any_reachable = false;
    for (card_id, status_port, control_port, carrier_addr, telephony_addr) in &lines {
        let reachable = print_line_live_status(
            card_id,
            &non_empty_or_loopback(carrier_addr),
            *status_port,
            &non_empty_or_loopback(telephony_addr),
            *control_port,
        );
        any_reachable = any_reachable || reachable;
    }
    any_reachable
}

/// Prints one line's live status block; returns whether its registration half
/// answered (a line whose carrier half failed to start is unreachable while
/// its siblings still report).
fn print_line_live_status(
    card_id: &str,
    carrier_addr: &str,
    status_port: u16,
    telephony_addr: &str,
    control_port: u16,
) -> bool {
    use crate::vowifi::control::ControlMessage;
    use crate::vowifi::{format_unix, query_status};

    println!("Line {card_id}:");
    let reg_addr = format!("{carrier_addr}:{status_port}");
    let reg = match query_status(&reg_addr) {
        Ok(ControlMessage::RegistrationStatusReply {
            state,
            registered_at,
            expires_at,
            last_failure,
            can_answer,
            blocked_reason,
        }) => (
            state,
            registered_at,
            expires_at,
            last_failure,
            can_answer,
            blocked_reason,
        ),
        // Unreachable, or an unexpected reply, both mean this line's carrier
        // half is not answering.
        _ => {
            println!("  registration: unreachable");
            return false;
        }
    };

    let (state, registered_at, expires_at, last_failure, can_answer, blocked_reason) = reg;
    println!("  registration:");
    println!("    state: {state}");
    println!("    registered_at: {}", format_unix(registered_at));
    println!("    expires_at: {}", format_unix(expires_at));
    match last_failure {
        Some((t, msg)) => println!("    last_failure: {} {msg}", format_unix(Some(t))),
        None => println!("    last_failure: none"),
    }
    // The one line an operator checks first (FR-014/FR-033): can this line
    // take a call right now, and if not, why — derived by the running service
    // from the same model that governs admission (`ims::lifecycle`).
    println!("    can_answer: {can_answer}");
    if let Some(reason) = blocked_reason {
        println!("    blocked_reason: {reason}");
    }

    println!("  recent calls:");
    let calls_addr = format!("{telephony_addr}:{control_port}");
    match query_status(&calls_addr) {
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
        // The registration half answered, so this line is up; a failure here
        // is just the telephone half briefly unreachable, reported inline
        // rather than falling back to the modem the service still owns.
        Ok(other) => println!("    unexpected reply: {other:?}"),
        Err(e) => println!("    unreachable: {e}"),
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_service_has_its_own_telephone_side_port() {
        // Two endpoints already raced for one; a third must not join them.
        assert_ne!(SIP_LOCAL_PORT, crate::vowifi::AGENT_B_SIP_LOCAL_PORT);
        assert_ne!(SIP_LOCAL_PORT, crate::vowifi::VETH_SIP_PORT);
        assert_ne!(SIP_LOCAL_PORT, crate::vowifi::AGENT_A_STATUS_PORT);
        let ports = [
            SIP_LOCAL_PORT,
            LOOPBACK_SIP_PORT,
            LOOPBACK_CONTROL_PORT,
            LOOPBACK_STATUS_PORT,
        ];
        for (i, a) in ports.iter().enumerate() {
            for b in &ports[i + 1..] {
                assert_ne!(a, b, "this service's own ports must not collide either");
            }
        }
    }

    #[test]
    fn per_line_port_trios_never_collide_with_the_shared_endpoint() {
        // The shared telephony endpoint stays fixed; every line's derived trio
        // must avoid it and every other line's trio (specs/018-volte-multi-modem).
        use super::super::discovery::{control_port, sip_leg_port, status_port};
        let mut seen = std::collections::HashSet::new();
        seen.insert(SIP_LOCAL_PORT);
        for i in 0..8u32 {
            for p in [sip_leg_port(i), control_port(i), status_port(i)] {
                assert!(seen.insert(p), "port {p} collides (line {i})");
            }
        }
    }
}
