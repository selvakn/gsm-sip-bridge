//! Bridging inbound cellular calls over the host-side LTE registration
//! (specs/017-volte-inbound-bridge, US1/US2).
//!
//! # One process, not two
//!
//! The Wi-Fi path splits into Agent A (carrier side) and Agent B (telephone
//! side) because the ePDG tunnel puts them in different network namespaces and
//! PJSIP cannot cross that boundary. **The LTE path has no namespace**
//! (specs/015 research R4), so that split buys nothing here.
//!
//! What it does *not* mean is reimplementing the call handling. `ims::agent`'s
//! INVITE handling, ringback, RTP relay and hangup propagation are the most
//! carefully-tuned code in the tree, and FR-019/SC-008 require one
//! implementation serving both paths. So this service reuses that logic
//! verbatim and drops only what the namespace forced:
//!
//! | Wi-Fi path | Here |
//! |---|---|
//! | Two processes | Two threads |
//! | veth pair | loopback |
//! | Agent B's own SIP port | a **third** local port ([`SIP_LOCAL_PORT`]) |
//!
//! The control protocol survives the merge. Over loopback it costs one socket
//! and saves forking the hardest code in the tree; replacing it with an
//! in-process channel would mean a second copy of `handle_invite`, which is
//! exactly what FR-019 exists to prevent.
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
use crate::ims::sdp;
use crate::ims::ImsRegisterConfig;
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

/// Card label used when none is supplied — the single-line case, mirroring
/// `vowifi::LEGACY_LINE_CARD_ID`.
pub const DEFAULT_CARD_ID: &str = "volte";

/// Loopback — both halves are threads in this process, so neither leg ever
/// leaves the host.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Everything the service needs to start.
pub struct ServiceConfig {
    /// Labels this line's metrics and call history.
    pub card_id: String,
    /// The LTE attachment this registration rides on.
    pub settings: VolteSettings,
    pub msisdn: Option<String>,
    /// Proceed even if the Wi-Fi path appears to hold the same subscriber's
    /// registration. An escape hatch for a stale detection, not a default.
    pub force: bool,
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
    // (FR-022).
    super::guard::check_no_vowifi_conflict(service.force).map_err(BridgeError::Ims)?;

    let attach = super::attach(&service.settings)?;
    tracing::info!(
        iface = %attach.iface,
        routed = attach.routed,
        "IMS PDN attached"
    );

    let pcscf = service
        .settings
        .pcscf
        .ok_or_else(|| BridgeError::Ims("no P-CSCF configured for the LTE IMS transport".into()))?;

    let plmn = {
        let mut at = crate::modules::at_commander::AtCommander::open(&service.settings.modem_port)?;
        crate::vowifi::plmn::derive_plmn(&mut at)?
    };

    let reg_cfg = ImsRegisterConfig {
        modem_port: service.settings.modem_port.clone(),
        pcscf_addr: pcscf.ip(),
        pcscf_port: pcscf.port(),
        mcc: plmn.mcc,
        mnc: plmn.mnc,
        imsi: None,
        imei: None,
        use_tcp: true,
        sec_agree: true,
        msisdn: service.msisdn.clone(),
        // Names the serving cell, so the network can apply the right policy
        // and an operator can tell which radio a call actually used.
        access_network_info: super::read_access_network_info(&service.settings.modem_port),
    };

    // The telephone-system half, on its own thread and its own SIP port. It
    // is the exact same code the Wi-Fi path runs; only the port and the
    // addresses differ.
    let telephony_line = crate::vowifi::RuntimeLine {
        index: 0,
        card_id: service.card_id.clone(),
        veth_local_addr: LOOPBACK.to_string(),
        veth_peer_addr: LOOPBACK.to_string(),
        control_port: LOOPBACK_CONTROL_PORT,
        sip_leg_port: LOOPBACK_SIP_PORT,
    };
    {
        let app_config = app_config.clone();
        std::thread::Builder::new()
            .name("volte-telephony".into())
            .spawn(move || {
                if let Err(e) = crate::vowifi::run_telephony_side(
                    &app_config,
                    SIP_LOCAL_PORT,
                    true,
                    vec![telephony_line],
                    "volte-bridge",
                    crate::store::Transport::Volte,
                ) {
                    tracing::error!(error = %e, "the telephone-side half stopped");
                }
            })
            .map_err(|e| BridgeError::Ims(format!("failed to start the telephone side: {e}")))?;
    }

    // Give the telephone-side half a moment to bind its control port before
    // the carrier side can be offered a call. A call arriving in this window
    // would otherwise fail its control connection and be declined — rare, but
    // it costs nothing to close.
    std::thread::sleep(TELEPHONY_STARTUP_GRACE);

    let control_addr = SocketAddr::new(LOOPBACK, LOOPBACK_CONTROL_PORT);

    // One lock guards every touch of the modem's AT port so two users never
    // interleave on it (research R6): registration renewal and re-attachment on
    // the carrier half, and the modem SMS reader spawned just below.
    let modem_lock = std::sync::Arc::new(std::sync::Mutex::new(()));

    // The circuit-switched SMS route (FR-036): the carrier may deliver a text
    // into the modem's own storage rather than as an IMS `MESSAGE`, and with
    // this card assigned exclusively here, nothing else reads that storage.
    // Its own thread, serialised with renewal through the shared lock; it
    // relays each message onto the same telephone-side recorder the IMS route
    // uses.
    {
        let modem_port = service.settings.modem_port.clone();
        let lock = modem_lock.clone();
        std::thread::Builder::new()
            .name("volte-sms-reader".into())
            .spawn(move || super::sms::run_modem_reader(modem_port, control_addr, lock))
            .map_err(|e| BridgeError::Ims(format!("failed to start the modem SMS reader: {e}")))?;
    }

    // Rebuilding the attachment is what must never happen mid-call. Passing it
    // as the renewal hook is what makes that true structurally — see
    // `ims::agent::PreRenewalHook`.
    let settings = service.settings.clone();
    let pre_renewal = move || super::registration::refresh_attachment(&settings);

    // FR-011: if the attachment genuinely dies mid-call, this is how the call
    // is ended with the cause stated. Reads `CEREG` under the shared modem lock
    // — only when the carrier leg has already gone silent, so it costs nothing
    // on a healthy call.
    let attach_modem = service.settings.modem_port.clone();
    let attach_lock = modem_lock.clone();
    let attachment_check = move || {
        let _guard = attach_lock.lock().unwrap_or_else(|e| e.into_inner());
        super::is_attached(&attach_modem)
    };

    crate::ims::agent::serve_inbound(crate::ims::agent::InboundParams {
        card_id: &service.card_id,
        reg_cfg: &reg_cfg,
        local_ip: LOOPBACK,
        control_addr,
        // An inbound call is a real conversation; the whole point of this path
        // is that it sounds better than the modem-internal one.
        wideband: true,
        answer_preference: sdp::AnswerPreference::cellular(),
        // Must equal the telephony line's `sip_leg_port` above. They are set
        // from the same constant so they cannot drift apart again.
        veth_sip_port: LOOPBACK_SIP_PORT,
        pre_renewal: Some(&pre_renewal),
        attachment_check: Some(&attachment_check),
        modem_lock: Some(modem_lock),
        app_config,
        agent_label: "volte-ims-agent",
        agent_kind: crate::control::protocol::AgentKind::Volte,
    })
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
/// Both halves run as loopback threads, so the carrier half's registration
/// listener is on [`crate::vowifi::AGENT_A_STATUS_PORT`] and the telephone
/// half's recent-call history is on [`LOOPBACK_CONTROL_PORT`] — the same query
/// `vowifi-status` makes of the Wi-Fi agents, pointed at loopback.
///
/// Returns `false` only when the registration half is unreachable, which is
/// what tells the caller the service is not running and a direct modem read is
/// safe.
pub fn print_live_status() -> bool {
    use crate::vowifi::control::ControlMessage;
    use crate::vowifi::{format_unix, query_status, AGENT_A_STATUS_PORT};

    let reg_addr = format!("{}:{AGENT_A_STATUS_PORT}", LOOPBACK);
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
        // Unreachable, or an unexpected reply, both mean "not the running
        // service" — let the caller fall back to the modem.
        _ => return false,
    };

    let (state, registered_at, expires_at, last_failure, can_answer, blocked_reason) = reg;
    println!("Live service (querying the running bridge, not the modem):");
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
    let calls_addr = format!("{}:{LOOPBACK_CONTROL_PORT}", LOOPBACK);
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
        // The registration half answered, so the service is up; a failure here
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
        let ports = [SIP_LOCAL_PORT, LOOPBACK_SIP_PORT, LOOPBACK_CONTROL_PORT];
        for (i, a) in ports.iter().enumerate() {
            for b in &ports[i + 1..] {
                assert_ne!(a, b, "this service's own ports must not collide either");
            }
        }
    }
}
