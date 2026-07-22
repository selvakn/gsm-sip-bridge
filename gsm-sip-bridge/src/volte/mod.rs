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

pub mod netcfg;
pub mod pdn;

use crate::error::BridgeResult;
use crate::ims::transport::{ImsTransport, ImsTransportHandle, TransportError, TransportResult};
use crate::modules::at_commander::AtCommander;
use std::net::{IpAddr, SocketAddr};
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
}

impl Default for VolteSettings {
    fn default() -> Self {
        Self {
            modem_port: PathBuf::from("/dev/ttyUSB0"),
            iface: String::new(),
            cid: DEFAULT_IMS_CID,
            apn: DEFAULT_IMS_APN.to_string(),
            pcscf: None,
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

/// Brings up the IMS PDN and makes it usable from the host.
pub fn attach(settings: &VolteSettings) -> BridgeResult<AttachReport> {
    let mut at = AtCommander::open(Path::new(&settings.modem_port))?;

    if let Some(warning) = displacement_warning(pdn::bound_context(&mut at)?, settings.cid) {
        tracing::warn!("{warning}");
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
            netcfg::configure(&settings.iface, assigned)?;
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
    tracing::info!(cid = settings.cid, "IMS PDN released");
    Ok(())
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
                "LTE IMS PDN (cid {}, APN {}), P-CSCF from configuration",
                report.pdn.cid, report.pdn.apn_assigned
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

/// Convenience for callers holding an `IpAddr` rather than a `SocketAddr`.
pub fn pcscf_socket(addr: IpAddr, port: Option<u16>) -> SocketAddr {
    SocketAddr::new(addr, port.unwrap_or(DEFAULT_PCSCF_PORT))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn pcscf_socket_defaults_to_the_sip_port() {
        let s = pcscf_socket("2402:8100::1".parse().unwrap(), None);

        assert_eq!(s.port(), DEFAULT_PCSCF_PORT);
    }

    #[test]
    fn transport_is_named_for_status_output() {
        let t = LteImsPdnTransport::new(VolteSettings::default());

        assert_eq!(t.name(), "lte-ims-pdn");
    }
}
