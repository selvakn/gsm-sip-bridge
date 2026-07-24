//! The per-line carrier-facing half (specs/020-volte-line-netns), extracted
//! from what used to be `volte::bridge::run_line`/`run_line_carrier`'s
//! in-process thread body without changing its logic — attach this line's
//! IMS PDN, derive its home PLMN, register over it, and answer calls until
//! the registration ends (`ims::agent::serve_inbound`).
//!
//! # Two homes, one function
//!
//! [`run`] is called from two places:
//!
//! - **The `volte-carrier-agent --line N` subcommand** (`main.rs`) — a whole
//!   OS process, launched by `docker/entrypoint.sh` via `ip netns exec
//!   <line's netns>`. This is the production, auto-discovered, isolated
//!   path (research.md R1/R3): the process inherits its namespace for every
//!   socket and every `ip`/`sysctl` shell-out `volte::netcfg`/`volte::pdn`
//!   make, with no per-call-site awareness needed.
//! - **`volte::bridge::run_inner`'s in-process thread**, for the
//!   single-`--modem` diagnostic invocation only (`volte-bridge --modem
//!   /dev/ttyUSBx`, run directly by an operator rather than
//!   `docker/entrypoint.sh`) — no namespace exists to isolate a manual
//!   one-off test into, so this keeps today's loopback-joined,
//!   same-process arrangement. `BridgeLine::veth_carrier_addr`/
//!   `veth_telephony_addr` are empty for this path, which is what selects
//!   `LOOPBACK` below.
//!
//! Neither caller retries on failure: the subcommand relies on
//! `docker/entrypoint.sh`'s per-line process supervision (matching how
//! `vowifi-ims-agent` is supervised, not an internal Rust retry loop); the
//! in-process diagnostic path keeps its own retry loop in `bridge::run_line`,
//! calling [`run`] once per attempt exactly as it called `run_line_carrier`
//! before the extraction.

use crate::config::AppConfig;
use crate::ims::sdp;
use crate::ims::ImsRegisterConfig;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use super::bridge::BridgeLine;

/// One attempt at a line's carrier half: attach its PDN, register over it,
/// and answer calls until the registration ends. Returns when the attempt is
/// over (failure or a lost registration) — the caller decides whether/how to
/// retry (see the module docs).
///
/// `pbx_registered` is `Some` only when this runs as an in-process thread
/// sharing the flag with the telephone-side half directly (the diagnostic
/// path) — `None` for the `volte-carrier-agent` subcommand, which is a
/// separate OS process and cannot share an `Arc`. This is the same
/// limitation `vowifi-ims-agent` already has for exactly the same reason
/// (`ims::agent`'s `InboundParams::pbx_registered` doc comment) — a carrier
/// agent process cannot fast-decline on a down PBX trunk and instead finds
/// out when the call setup itself fails. Closing that gap would need a new
/// cross-process status query and is tracked separately; it is not a
/// regression this feature introduces so much as one it does not yet close
/// for VoLTE's shared-trunk case the way VoWiFi's independent-trunk-per-line
/// model never needed to.
pub fn run(
    line: &BridgeLine,
    app_config: &AppConfig,
    modem_lock: Arc<Mutex<()>>,
    pbx_registered: Option<Arc<AtomicBool>>,
) {
    // Attach and PLMN derivation both touch the AT port, so hold the modem
    // lock across them to stay clear of the SMS reader running concurrently.
    // `attach` records the displaced context (via `settings.restore_cid_path`)
    // *before* it rebinds, so the container's cleanup can restore it even if
    // this line is killed mid-attach.
    let plmn = {
        let _guard = modem_lock.lock().unwrap_or_else(|e| e.into_inner());
        let attach = match super::attach(&line.settings) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(card_id = %line.card_id, error = %e, "line failed to attach its IMS PDN");
                return;
            }
        };
        tracing::info!(
            card_id = %line.card_id,
            iface = %attach.iface,
            routed = attach.routed,
            "IMS PDN attached"
        );
        let mut at = match crate::modules::at_commander::AtCommander::open(
            &line.settings.modem_port,
        ) {
            Ok(at) => at,
            Err(e) => {
                tracing::error!(card_id = %line.card_id, error = %e, "could not open the modem to derive the PLMN");
                return;
            }
        };
        match crate::vowifi::plmn::derive_plmn(&mut at) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(card_id = %line.card_id, error = %e, "could not derive the home PLMN");
                return;
            }
        }
    };

    let Some(pcscf) = line.settings.pcscf else {
        tracing::error!(card_id = %line.card_id, "no P-CSCF configured for this line");
        return;
    };

    let reg_cfg = ImsRegisterConfig {
        modem_port: line.settings.modem_port.clone(),
        pcscf_addr: pcscf.ip(),
        pcscf_port: pcscf.port(),
        mcc: plmn.mcc,
        mnc: plmn.mnc,
        imsi: None,
        imei: None,
        use_tcp: true,
        sec_agree: true,
        msisdn: line.msisdn.clone(),
        // Names the serving cell, so the network can apply the right policy
        // and an operator can tell which radio a call actually used.
        access_network_info: super::read_access_network_info(&line.settings.modem_port),
    };

    // Rebuilding the attachment is what must never happen mid-call. Passing it
    // as the renewal hook is what makes that true structurally — see
    // `ims::agent::PreRenewalHook`.
    let settings = line.settings.clone();
    let pre_renewal = move || super::registration::refresh_attachment(&settings);

    // FR-011: if the attachment genuinely dies mid-call, this is how the call
    // is ended with the cause stated. Reads `CEREG` under the shared modem lock
    // — only when the carrier leg has already gone silent, so it costs nothing
    // on a healthy call.
    let attach_modem = line.settings.modem_port.clone();
    let attach_lock = modem_lock.clone();
    let attachment_check = move || {
        let _guard = attach_lock.lock().unwrap_or_else(|e| e.into_inner());
        super::is_attached(&attach_modem)
    };

    // Empty veth address (the diagnostic single-`--modem` path, no namespace)
    // means "both halves are one process/namespace" — loopback, exactly as
    // before this feature. A real address (the netns-isolated production
    // path) is this line's own carrier-side veth end, which Agent B reaches
    // over the veth link the same way it already reaches VoWiFi's Agent A
    // (research.md R2).
    let local_ip: IpAddr = if line.veth_carrier_addr.is_empty() {
        super::bridge::LOOPBACK
    } else {
        match line.veth_carrier_addr.parse() {
            Ok(ip) => ip,
            Err(e) => {
                tracing::error!(card_id = %line.card_id, addr = %line.veth_carrier_addr, error = %e, "invalid veth_carrier_addr for this line");
                return;
            }
        }
    };
    let telephony_ip: IpAddr = if line.veth_telephony_addr.is_empty() {
        super::bridge::LOOPBACK
    } else {
        match line.veth_telephony_addr.parse() {
            Ok(ip) => ip,
            Err(e) => {
                tracing::error!(card_id = %line.card_id, addr = %line.veth_telephony_addr, error = %e, "invalid veth_telephony_addr for this line");
                return;
            }
        }
    };
    let control_addr = SocketAddr::new(telephony_ip, line.control_port);

    if let Err(e) = crate::ims::agent::serve_inbound(crate::ims::agent::InboundParams {
        card_id: &line.card_id,
        reg_cfg: &reg_cfg,
        local_ip,
        control_addr,
        status_port: line.status_port,
        // An inbound call is a real conversation; the whole point of this path
        // is that it sounds better than the modem-internal one.
        wideband: true,
        answer_preference: sdp::AnswerPreference::cellular(),
        // Must equal the telephony line's `sip_leg_port`. They come from this
        // line's single derivation so they cannot drift apart.
        veth_sip_port: line.sip_leg_port,
        pre_renewal: Some(&pre_renewal),
        attachment_check: Some(&attachment_check),
        modem_lock: Some(modem_lock),
        pbx_registered,
        app_config,
        agent_label: "volte-ims-agent",
        agent_kind: crate::control::protocol::AgentKind::Volte,
    }) {
        tracing::error!(card_id = %line.card_id, error = %e, "the carrier half for this line stopped");
    }
}
