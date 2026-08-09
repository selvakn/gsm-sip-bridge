//! `volte probe-inbound`: does the carrier route mobile-terminating calls to
//! this registration at all?
//!
//! Split out of `agent::mod` because it is a diagnostic, not part of the
//! bridge — it registers, listens, reports, and deliberately answers nothing.
//! In the pre-split file it sat *below* the test module, which made it easy to
//! miss entirely.

use crate::error::{BridgeError, BridgeResult};
use crate::ims::session::{start_inbound, subscribe_reg_event};
use crate::ims::sip_client::{build_100_trying, build_486_busy_here, random_hex, SipMessage};
use std::time::{Duration, Instant};

/// What an inbound probe observed.
#[derive(Debug, Default)]
pub struct InboundProbeReport {
    pub invites: u32,
    pub other_requests: u32,
    /// True once anything at all arrives on the protected port. The probe's
    /// positive control: without it, "no incoming call" is uninterpretable.
    pub port_proven_reachable: bool,
    /// Method and caller for each request the network delivered, in order.
    pub log: Vec<String>,
}

/// Registers, holds the protected server port open, and reports everything the
/// network delivers (specs/017-volte-inbound-bridge).
///
/// This answers that feature's gating question: **does the carrier route
/// mobile-terminating calls to us over this registration at all?** Registration
/// works and reg-event notifications already arrive, but an inbound `INVITE`
/// has never been observed on the LTE path — and if it never arrives, the
/// feature is not buildable rather than merely delayed.
///
/// Deliberately does not answer calls. An `INVITE` is acknowledged and then
/// declined with `486 Busy Here`, so the caller gets a clean, immediate result
/// instead of ringing at nothing — the probe is establishing reachability, not
/// carrying a conversation.
pub fn probe_inbound(
    cfg: &crate::ims::ImsRegisterConfig,
    listen_for: Duration,
) -> BridgeResult<InboundProbeReport> {
    let mut session = crate::ims::register_session(cfg)?;
    if session.status != 200 {
        let (status, reason) = (session.status, session.reason.clone());
        session.unregister();
        session.cleanup();
        return Err(BridgeError::Ims(format!(
            "registration failed, so nothing could be delivered to us: {status} {reason}"
        )));
    }

    // Positive control. Without this the probe has no way to tell "the carrier
    // does not route calls to us" from "our protected port is unreachable" —
    // and those demand completely different responses. A reg-event
    // notification arriving proves the network can reach us, which is what
    // makes a subsequent *absent* INVITE meaningful evidence.
    let inbound = start_inbound(&session)?;
    subscribe_reg_event(&mut session);
    match session.gm_server_addr() {
        Some(addr) => tracing::info!(
            %addr,
            "registered — listening for network-initiated requests. Dial the SIM now."
        ),
        None => tracing::warn!(
            "registered, but with no protected server port — the network has nowhere to \
             deliver an inbound call"
        ),
    }

    let mut report = InboundProbeReport::default();
    let deadline = Instant::now() + listen_for;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok((msg, sink)) = inbound
            .rx
            .recv_timeout(remaining.min(Duration::from_secs(2)))
        else {
            continue;
        };
        let SipMessage::Request(req) = msg else {
            continue;
        };
        let from = req.header("From").unwrap_or("<unknown>").to_string();
        report.port_proven_reachable = true;
        let entry = format!("{} from {}", req.method, from);
        tracing::info!(method = %req.method, from = %from, "network delivered a request");
        report.log.push(entry);

        if req.method.eq_ignore_ascii_case("INVITE") {
            report.invites += 1;
            // Acknowledge, then decline: the caller gets an immediate busy
            // rather than ringing at a probe that will never answer.
            let _ = sink.send(&build_100_trying(&req));
            let _ = sink.send(&build_486_busy_here(&req, &random_hex(4)));
        } else {
            report.other_requests += 1;
        }
    }

    drop(inbound);
    session.unregister();
    session.cleanup();
    Ok(report)
}
