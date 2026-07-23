//! Host-side IMS over LTE (specs/015-volte-host-ims).
//!
//! The bridge's own IMS stack, reached over an LTE IMS PDN instead of the
//! ePDG tunnel the VoWiFi path uses. Deliberately mirrors the shape of
//! `crate::vowifi` so the two transports read as parallel.
//!
//! Everything above the network attachment — registration, IMS-AKA, Gm
//! IPsec, SIP message construction — is *shared*, not reimplemented: both
//! paths satisfy `crate::ims::transport::ImsTransport` and feed the same
//! `crate::ims` machinery.
//!
//! ## Scope
//!
//! This module currently delivers US1 (the IMS PDN attachment) and the
//! transport shell around it. Registration over LTE is blocked on obtaining a
//! P-CSCF address at all: the Gate G1 investigation established that this
//! carrier publishes one by no mechanism the host can reach — DHCPv6 replies
//! with no RFC 3319 options, the RA carries only prefix and MTU, and no
//! usable resolver is offered. See `research.md` R2 and `plan.md` Gate G3.
//! Until that is resolved, a P-CSCF must be supplied explicitly.

pub mod bridge;
pub mod guard;
pub mod netcfg;
pub mod pani;
pub mod pcscf;
pub mod pdn;
pub mod registration;
pub mod sms;

// `read_access_network_info` is called as `volte::read_access_network_info`
// from `main` and `bridge`, so re-export it at the module root where it has
// always lived even though its implementation now sits in `pani`.
pub use pani::read_access_network_info;

use crate::error::BridgeResult;
use crate::ims::transport::{ImsTransport, ImsTransportHandle, TransportError, TransportResult};
use crate::modules::at_commander::AtCommander;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// Default PDP context id for the IMS PDN. Chosen to sit clear of the
/// contexts the modem defines for general internet access (1 and 2 on the
/// reference hardware).
pub const DEFAULT_IMS_CID: u8 = 3;

/// Default APN. The network resolves this to its own fully-qualified name
/// (e.g. `ims.mnc043.mcc404.gprs`), which is what gets reported back.
pub const DEFAULT_IMS_APN: &str = "ims";

/// Default SIP port for the P-CSCF.
pub const DEFAULT_PCSCF_PORT: u16 = 5060;

/// Settings for the LTE IMS transport.
#[derive(Debug, Clone)]
pub struct VolteSettings {
    pub modem_port: PathBuf,
    /// Host network interface bound to the IMS PDN.
    pub iface: String,
    pub cid: u8,
    pub apn: String,
    /// Explicitly configured P-CSCF. Required today — see the module docs.
    pub pcscf: Option<SocketAddr>,
    /// Where to record the context id this attach displaces, so an external
    /// teardown can `--restore-cid` it. Written *before* the displacing rebind,
    /// so a crash mid-attach still leaves the value for cleanup. `None` for
    /// callers that run their own detach (they already hold the displaced cid).
    pub restore_cid_path: Option<PathBuf>,
}

impl Default for VolteSettings {
    fn default() -> Self {
        Self {
            modem_port: PathBuf::from("/dev/ttyUSB0"),
            iface: String::new(),
            cid: DEFAULT_IMS_CID,
            apn: DEFAULT_IMS_APN.to_string(),
            pcscf: None,
            restore_cid_path: None,
        }
    }
}

/// What `bring_up` achieved, for operator reporting (FR-003, FR-006).
#[derive(Debug, Clone)]
pub struct AttachReport {
    pub pdn: pdn::ImsPdn,
    pub iface: String,
    pub reused: bool,
    pub displaced_cid: Option<u8>,
    /// Global addresses present on the interface after configuration.
    pub global_addresses: Vec<std::net::Ipv6Addr>,
    /// Whether a default route via the interface exists — i.e. whether the
    /// carrier's router advertisement was actually accepted. **This, not the
    /// presence of an address, is what separates "attached" from "usable":**
    /// the assigned address is installed by us regardless.
    pub routed: bool,
}

impl AttachReport {
    /// Operator-facing summary. Deliberately names the *assigned* APN and the
    /// bearer id: together they are the evidence that the carrier granted a
    /// real IMS PDN rather than quietly reusing the default bearer.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        if self.reused {
            s.push_str("IMS PDN already attached; reusing it.\n");
        } else {
            s.push_str("IMS PDN attached.\n");
        }
        s.push_str(&format!("  context id     : {}\n", self.pdn.cid));
        s.push_str(&format!("  APN requested  : {}\n", self.pdn.apn_requested));
        s.push_str(&format!("  APN assigned   : {}\n", self.pdn.apn_assigned));
        s.push_str(&format!("  bearer id      : {}\n", self.pdn.bearer_id));
        s.push_str(&format!("  address family : {}\n", self.pdn.family()));
        if let Some(v6) = self.pdn.ipv6 {
            s.push_str(&format!("  address        : {v6}\n"));
        }
        if let Some(v4) = self.pdn.ipv4 {
            s.push_str(&format!("  address (v4)   : {v4}\n"));
        }
        s.push_str(&format!("  host interface : {}\n", self.iface));
        if !self.global_addresses.is_empty() {
            s.push_str("  host addresses : ");
            s.push_str(
                &self
                    .global_addresses
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            s.push('\n');
        }
        if !self.iface.is_empty() {
            s.push_str(&format!(
                "  routable       : {}\n",
                if self.routed {
                    "yes (default route via the carrier)"
                } else {
                    "NO — no default route; the router advertisement was not accepted"
                }
            ));
        }
        if let Some(prev) = self.displaced_cid {
            s.push_str(&format!(
                "  NOTE: the host data path was re-pointed from context {prev} to {}; \
                 general connectivity through the modem is displaced until teardown.\n",
                self.pdn.cid
            ));
        }
        s
    }
}

/// Warning issued *before* the data path is re-pointed (FR-006).
pub fn displacement_warning(current: Option<u8>, target: u8) -> Option<String> {
    match current {
        Some(prev) if prev != target => Some(format!(
            "the modem exposes a single host data path; binding context {target} will \
             displace context {prev} and drop general connectivity through the modem"
        )),
        _ => None,
    }
}

/// What to do with the restore-cid record on an attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreCid {
    /// Record this context — it is the one the attach is about to displace.
    Record(u8),
    /// Leave any existing record untouched.
    Keep,
}

/// Decides how the restore-cid record should change for an attach, given the
/// context bound *before* it (`prior_cid`) and the IMS context being bound
/// (`ims_cid`). Pure, so the rule can be tested without a modem.
///
/// Records only a *genuine* displacement (a different prior context); anything
/// else keeps the existing record. In particular **`None` (nothing bound) keeps
/// it, never clears it**: that state occurs transiently when a renewal
/// re-attaches after the carrier drops the PDN, and clearing there would erase
/// the original displacement so final teardown could not restore it. A single
/// container lifetime shares one tmpfs record, and the modem is dedicated, so a
/// kept record always names the pre-IMS context — there is no stale value to
/// worry about, only a valid one to protect.
fn restore_cid_action(prior_cid: Option<u8>, ims_cid: u8) -> RestoreCid {
    match prior_cid {
        Some(prior) if prior != ims_cid => RestoreCid::Record(prior),
        _ => RestoreCid::Keep,
    }
}

/// Brings up the IMS PDN and makes it usable from the host.
pub fn attach(settings: &VolteSettings) -> BridgeResult<AttachReport> {
    let mut at = AtCommander::open(Path::new(&settings.modem_port))?;

    let prior_cid = pdn::bound_context(&mut at)?;
    if let Some(warning) = displacement_warning(prior_cid, settings.cid) {
        tracing::warn!("{warning}");
    }
    // Record (or clear) the context to rebind on teardown, *before* `bring_up`
    // rebinds the host data path — otherwise a crash/shutdown in the window
    // between the rebind and this function returning would leave the teardown
    // unable to `--restore-cid`, stranding general connectivity (found by
    // review). The record must reflect *this* attach so a supervised restart
    // never restores a stale context:
    if let Some(path) = &settings.restore_cid_path {
        match restore_cid_action(prior_cid, settings.cid) {
            RestoreCid::Record(prior) => {
                if let Err(e) = std::fs::write(path, prior.to_string()) {
                    tracing::warn!(path = %path.display(), error = %e,
                        "could not record the displaced context id; teardown will not restore it");
                }
            }
            RestoreCid::Keep => {}
        }
    }

    let brought_up = pdn::bring_up(&mut at, settings.cid, &settings.apn)?;
    tracing::info!(
        cid = brought_up.pdn.cid,
        apn_assigned = %brought_up.pdn.apn_assigned,
        bearer_id = brought_up.pdn.bearer_id,
        family = brought_up.pdn.family(),
        "IMS PDN established"
    );

    let mut global_addresses = Vec::new();
    let mut routed = false;
    if !settings.iface.is_empty() {
        if let Some(assigned) = brought_up.pdn.ipv6 {
            // FR-024. Without this the PDN is bound but unusable — the
            // carrier's router advertisements never reach us.
            // The modem's data path needs a moment after QNETDEVCTL before the
            // host interface has carrier. Configuring first would leave the
            // link-local stuck tentative, since DAD cannot run without it.
            if !netcfg::wait_for_carrier(&settings.iface, std::time::Duration::from_secs(10)) {
                tracing::warn!(
                    iface = %settings.iface,
                    "no carrier before configuring; the modem may not have finished binding"
                );
            }
            netcfg::configure(&settings.iface, assigned)?;
            // `configure` toggles the link, so wait for carrier again before
            // expecting duplicate address detection to make progress.
            netcfg::wait_for_carrier(&settings.iface, std::time::Duration::from_secs(10));
            // The kernel sends no Router Solicitation while the link-local is
            // tentative, so soliciting before DAD finishes is simply ignored.
            if !netcfg::wait_for_link_local(&settings.iface, std::time::Duration::from_secs(8))? {
                tracing::warn!(
                    iface = %settings.iface,
                    "the link-local address did not complete duplicate address detection"
                );
            }
            netcfg::solicit_router(&settings.iface)?;
            // The default route, not the address, is what proves the RA was
            // accepted — we installed the address ourselves.
            routed = netcfg::wait_for_router(&settings.iface, std::time::Duration::from_secs(15))?;
            global_addresses = netcfg::global_addresses(&settings.iface)?;
            if !routed {
                tracing::warn!(
                    iface = %settings.iface,
                    "the interface is configured but no default route appeared; \
                     the carrier's router advertisement was not accepted, so the \
                     PDN is attached but cannot carry traffic"
                );
            }
        } else {
            tracing::warn!("the PDN has no IPv6 address; skipping host interface configuration");
        }
    }

    // Attached-but-unroutable is not "up": see research.md R10.
    crate::metrics::VOLTE_PDN_UP.set(if routed { 1.0 } else { 0.0 });

    Ok(AttachReport {
        pdn: brought_up.pdn,
        iface: settings.iface.clone(),
        reused: brought_up.reused,
        displaced_cid: brought_up.displaced_cid,
        global_addresses,
        routed,
    })
}

/// Releases the IMS PDN and reverts host configuration (FR-005).
pub fn detach(settings: &VolteSettings, restore_cid: Option<u8>) -> BridgeResult<()> {
    if !settings.iface.is_empty() {
        netcfg::teardown(&settings.iface)?;
    }
    let mut at = AtCommander::open(Path::new(&settings.modem_port))?;
    pdn::tear_down(&mut at, settings.cid, restore_cid)?;
    crate::metrics::VOLTE_PDN_UP.set(0.0);
    tracing::info!(cid = settings.cid, "IMS PDN released");
    Ok(())
}

/// True while the modem reports it is attached to the packet domain — `CEREG`
/// stat 1 (registered, home) or 5 (registered, roaming). Used as the mid-call
/// attachment check (FR-011).
///
/// **Deliberately biased toward "attached" on any doubt.** A false "not
/// attached" would tear down a live call; a false "attached" only delays
/// noticing a real loss by one probe. So a port that will not open, a `CEREG`
/// line that will not parse, or a read error all read as attached — only a
/// clear, parsed not-registered state (0/2/3/4) reports the attachment down.
pub fn is_attached(modem_port: &Path) -> bool {
    use crate::modules::at_commander::AtResponse;
    let Ok(mut at) = AtCommander::open(modem_port) else {
        return true;
    };
    match at.send_command("AT+CEREG?") {
        Ok(AtResponse::Ok(lines)) => match pdn::parse_cereg_stat(&lines) {
            Some(1) | Some(5) => true,
            Some(_) => false,
            None => true,
        },
        _ => true,
    }
}

/// Current attachment state, without changing anything.
pub fn status(settings: &VolteSettings) -> BridgeResult<Option<AttachReport>> {
    let mut at = AtCommander::open(Path::new(&settings.modem_port))?;
    if !pdn::is_active(&mut at, settings.cid)? {
        return Ok(None);
    }
    let Some(pdn) = pdn::read_pdn(&mut at, settings.cid, &settings.apn)? else {
        return Ok(None);
    };
    let bound = pdn::bound_context(&mut at)?;
    let (global_addresses, routed) = if settings.iface.is_empty() {
        (Vec::new(), false)
    } else {
        (
            netcfg::global_addresses(&settings.iface).unwrap_or_default(),
            netcfg::has_default_route(&settings.iface).unwrap_or(false),
        )
    };
    Ok(Some(AttachReport {
        pdn,
        iface: settings.iface.clone(),
        // A status query observes an existing attachment; it never creates
        // one, so this is always a pre-existing PDN.
        reused: true,
        displaced_cid: bound.filter(|c| *c != settings.cid),
        global_addresses,
        routed,
    }))
}

/// The LTE IMS PDN as an `ImsTransport`, the counterpart to
/// `crate::ims::transport::EpdgTransport`.
pub struct LteImsPdnTransport {
    settings: VolteSettings,
    prepared: Option<ImsTransportHandle>,
    displaced_cid: Option<u8>,
}

impl LteImsPdnTransport {
    pub fn new(settings: VolteSettings) -> Self {
        Self {
            settings,
            prepared: None,
            displaced_cid: None,
        }
    }
}

impl ImsTransport for LteImsPdnTransport {
    fn prepare(&mut self) -> TransportResult<ImsTransportHandle> {
        if let Some(handle) = &self.prepared {
            return Ok(handle.clone());
        }

        let report =
            attach(&self.settings).map_err(|e| TransportError::attaching(e.to_string()))?;
        self.displaced_cid = report.displaced_cid;

        // Automatic discovery is not viable on the tested carrier (Gate G1);
        // an explicit address is currently the only working source. Failing
        // here — at the discovery stage, with the reason — is far more useful
        // than a later, unexplained registration timeout.
        let pcscf = self.settings.pcscf.ok_or_else(|| {
            TransportError::discovering_pcscf(
                "no P-CSCF address configured, and this carrier publishes none by any \
                 mechanism reachable from the host (DHCPv6 returns no RFC 3319 options, \
                 the router advertisement carries none, and no resolver is offered). \
                 Supply one explicitly — see specs/015-volte-host-ims/plan.md Gate G3",
            )
        })?;

        let handle = ImsTransportHandle {
            pcscf,
            descriptor: format!(
                "LTE IMS PDN (cid {}, APN {}, {}), P-CSCF from configuration",
                report.pdn.cid,
                report.pdn.apn_assigned,
                if report.routed {
                    "routable"
                } else {
                    "NOT routable"
                }
            ),
        };
        self.prepared = Some(handle.clone());
        Ok(handle)
    }

    fn teardown(&mut self) -> TransportResult<()> {
        // Safe after a partially-failed prepare: detach tolerates a PDN that
        // was never brought up.
        let result = detach(&self.settings, self.displaced_cid);
        self.prepared = None;
        self.displaced_cid = None;
        result.map_err(|e| TransportError::attaching(e.to_string()))
    }

    fn name(&self) -> &'static str {
        "lte-ims-pdn"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_cid_records_a_genuine_displacement() {
        // Bound to the general-internet context (1) before, binding IMS (3):
        // record 1 so teardown rebinds it.
        assert_eq!(restore_cid_action(Some(1), 3), RestoreCid::Record(1));
    }

    #[test]
    fn restore_cid_keeps_the_record_when_reusing_our_pdn() {
        // A supervised restart finds the IMS context already bound (reuse). The
        // original displacement still applies, so don't overwrite the record.
        assert_eq!(restore_cid_action(Some(3), 3), RestoreCid::Keep);
    }

    #[test]
    fn restore_cid_keeps_the_record_when_nothing_is_bound() {
        // Nothing bound happens transiently when a renewal re-attaches after the
        // carrier drops the PDN. Clearing there would erase the original
        // displacement, so final teardown could not restore it — keep it.
        assert_eq!(restore_cid_action(None, 3), RestoreCid::Keep);
    }

    fn sample_pdn() -> pdn::ImsPdn {
        pdn::ImsPdn {
            cid: 3,
            apn_requested: "ims".into(),
            apn_assigned: "ims.mnc043.mcc404.gprs".into(),
            bearer_id: 6,
            ipv4: None,
            ipv6: Some("2402:8100:6ffe:8ae6:0:c:de2b:3801".parse().unwrap()),
        }
    }

    #[test]
    fn warns_before_displacing_an_existing_binding() {
        let w = displacement_warning(Some(1), 3).expect("expected a warning");

        assert!(w.contains("displace context 1"));
    }

    #[test]
    fn does_not_warn_when_rebinding_the_same_context() {
        assert_eq!(displacement_warning(Some(3), 3), None);
    }

    #[test]
    fn does_not_warn_when_nothing_was_bound() {
        assert_eq!(displacement_warning(None, 3), None);
    }

    #[test]
    fn summary_reports_the_evidence_the_pdn_is_genuinely_ims() {
        let r = AttachReport {
            pdn: sample_pdn(),
            iface: "enx0".into(),
            reused: false,
            displaced_cid: None,
            global_addresses: vec!["2402:8100:6ffe:8ae6:4b:b3ff:feb9:ebe5".parse().unwrap()],
            routed: true,
        };

        let s = r.summary();

        assert!(s.contains("ims.mnc043.mcc404.gprs"), "assigned APN missing");
        assert!(s.contains("bearer id      : 6"), "bearer id missing");
        assert!(s.contains("IPv6-only"), "family missing");
        assert!(s.contains("2402:8100:6ffe:8ae6:4b:b3ff:feb9:ebe5"));
    }

    #[test]
    fn summary_flags_reuse_rather_than_claiming_a_new_attachment() {
        let r = AttachReport {
            pdn: sample_pdn(),
            iface: "enx0".into(),
            reused: true,
            displaced_cid: None,
            global_addresses: vec![],
            routed: true,
        };

        assert!(r.summary().contains("already attached"));
    }

    #[test]
    fn summary_names_the_displaced_context() {
        let r = AttachReport {
            pdn: sample_pdn(),
            iface: "enx0".into(),
            reused: false,
            displaced_cid: Some(1),
            global_addresses: vec![],
            routed: true,
        };

        let s = r.summary();
        assert!(s.contains("re-pointed from context 1"));
    }

    #[test]
    fn transport_without_a_configured_pcscf_fails_at_the_discovery_stage() {
        // The failure must be attributable, and must explain *why* discovery
        // is not attempted, rather than surfacing as a registration timeout.
        use crate::ims::transport::TransportStage;

        let mut t = LteImsPdnTransport::new(VolteSettings {
            // A path that cannot open, so attach() fails before any hardware
            // is touched; the point under test is the stage tagging.
            modem_port: PathBuf::from("/nonexistent/tty"),
            ..VolteSettings::default()
        });

        let err = t.prepare().unwrap_err();

        assert_eq!(err.stage, TransportStage::Attaching);
    }

    #[test]
    fn transport_is_named_for_status_output() {
        let t = LteImsPdnTransport::new(VolteSettings::default());

        assert_eq!(t.name(), "lte-ims-pdn");
    }
}
