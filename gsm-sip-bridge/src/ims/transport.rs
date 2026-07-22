//! The seam that lets one registration implementation serve two access
//! networks (specs/015-volte-host-ims, `contracts/ims-transport-contract.md`).
//!
//! Registration, IMS-AKA, and Gm IPsec are identical whether the bridge
//! reaches the carrier's IMS core over an ePDG tunnel (VoWiFi) or over an LTE
//! IMS PDN (VoLTE) — only the network attachment underneath differs. An
//! `ImsTransport` is responsible for *producing a network position from which
//! IMS signalling can reach the P-CSCF*, and for tearing it down; everything
//! above that is shared.
//!
//! Deliberately small. The ePDG side has no attachment work to do at all —
//! `docker/entrypoint.sh` and the tunnel dialer establish the tunnel long
//! before any agent starts, and the dialer drops the P-CSCF it learned from
//! the IKEv2 config payload into a file. So `EpdgTransport::prepare` is just
//! that file read, which is exactly what `agent.rs` did inline before this
//! trait existed. That equivalence is the point: adopting the trait must not
//! change VoWiFi's behaviour in any observable way (FR-019).

use crate::error::{BridgeError, BridgeResult};
use std::net::{IpAddr, SocketAddr};

/// Where a transport failed, so a failure survives the abstraction boundary
/// with enough context for `FR-015` to distinguish "the network attachment
/// never came up" from "we never learned where to send the REGISTER".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportStage {
    /// Establishing the underlying network attachment (tunnel, or IMS PDN).
    Attaching,
    /// Determining the P-CSCF address to register against.
    DiscoveringPcscf,
}

impl TransportStage {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportStage::Attaching => "attaching",
            TransportStage::DiscoveringPcscf => "discovering-pcscf",
        }
    }
}

/// A transport failure tagged with the stage it happened at.
#[derive(Debug, Clone)]
pub struct TransportError {
    pub stage: TransportStage,
    pub detail: String,
}

impl TransportError {
    pub fn attaching(detail: impl Into<String>) -> Self {
        Self {
            stage: TransportStage::Attaching,
            detail: detail.into(),
        }
    }

    pub fn discovering_pcscf(detail: impl Into<String>) -> Self {
        Self {
            stage: TransportStage::DiscoveringPcscf,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.stage.as_str(), self.detail)
    }
}

impl std::error::Error for TransportError {}

impl From<TransportError> for BridgeError {
    fn from(e: TransportError) -> Self {
        BridgeError::Ims(e.to_string())
    }
}

pub type TransportResult<T> = Result<T, TransportError>;

/// What a prepared transport hands to the registration machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImsTransportHandle {
    /// Where to send SIP.
    pub pcscf: SocketAddr,
    /// How this transport reached that address — carried into logs and
    /// operator-facing status so "which P-CSCF, and where did it come from"
    /// is always answerable (FR-009).
    pub descriptor: String,
}

/// Produces a network position from which IMS signalling can reach the
/// carrier's P-CSCF.
///
/// Contract (see `contracts/ims-transport-contract.md`):
/// - `prepare` is **idempotent** — calling it twice yields one attachment,
///   not two.
/// - `teardown` reverts everything `prepare` applied, is safe after a
///   partially-failed `prepare`, and is safe to call twice.
/// - Errors carry the `TransportStage` they occurred at.
pub trait ImsTransport {
    fn prepare(&mut self) -> TransportResult<ImsTransportHandle>;

    fn teardown(&mut self) -> TransportResult<()>;

    /// Stable name for logs and status output.
    fn name(&self) -> &'static str;
}

/// The VoWiFi transport: the ePDG tunnel, established out-of-band by
/// `docker/entrypoint.sh` before the agent starts.
///
/// This owns no attachment lifecycle of its own — the tunnel outlives any
/// single registration and is supervised elsewhere — so `teardown` is a
/// no-op. Tearing the tunnel down here would be actively wrong: it is shared
/// with the SIP-side agent and survives agent restarts by design.
pub struct EpdgTransport {
    pcscf_source_path: String,
    pcscf_port: u16,
    prepared: Option<ImsTransportHandle>,
}

impl EpdgTransport {
    pub fn new(pcscf_source_path: impl Into<String>, pcscf_port: u16) -> Self {
        Self {
            pcscf_source_path: pcscf_source_path.into(),
            pcscf_port,
            prepared: None,
        }
    }

    /// Reads the P-CSCF address the tunnel dialer learned from the IKEv2
    /// config payload. Byte-for-byte the behaviour `agent.rs::read_pcscf` had
    /// before the trait existed, including the error text.
    fn read_pcscf(&self) -> TransportResult<IpAddr> {
        let path = &self.pcscf_source_path;
        let raw = std::fs::read_to_string(path).map_err(|e| {
            TransportError::discovering_pcscf(format!(
                "failed to read P-CSCF address from {path}: {e}"
            ))
        })?;
        raw.trim().parse().map_err(|e| {
            TransportError::discovering_pcscf(format!("invalid P-CSCF address in {path}: {e}"))
        })
    }
}

impl ImsTransport for EpdgTransport {
    fn prepare(&mut self) -> TransportResult<ImsTransportHandle> {
        // Idempotent: the tunnel is already up and the P-CSCF file does not
        // change under us within a run, so a second prepare returns the same
        // handle rather than re-reading.
        if let Some(handle) = &self.prepared {
            return Ok(handle.clone());
        }
        let addr = self.read_pcscf()?;
        let handle = ImsTransportHandle {
            pcscf: SocketAddr::new(addr, self.pcscf_port),
            descriptor: format!("ePDG tunnel, P-CSCF from {}", self.pcscf_source_path),
        };
        self.prepared = Some(handle.clone());
        Ok(handle)
    }

    fn teardown(&mut self) -> TransportResult<()> {
        // The tunnel is not ours to tear down (see the type-level comment).
        // Forgetting the cached handle is the whole of our state.
        self.prepared = None;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "epdg"
    }
}

/// Convenience for callers that just want the address and are happy to
/// surface a transport failure as a plain `BridgeError`.
pub fn prepare_pcscf<T: ImsTransport>(transport: &mut T) -> BridgeResult<SocketAddr> {
    Ok(transport.prepare()?.pcscf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_pcscf_file(contents: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "pcscf-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn epdg_prepare_reads_the_pcscf_the_dialer_wrote() {
        let path = temp_pcscf_file("10.1.2.3\n");
        let mut t = EpdgTransport::new(path.to_string_lossy(), 5060);

        let handle = t.prepare().unwrap();

        assert_eq!(handle.pcscf, "10.1.2.3:5060".parse().unwrap());
        assert!(handle.descriptor.contains("ePDG"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn epdg_prepare_is_idempotent() {
        let path = temp_pcscf_file("10.1.2.3");
        let mut t = EpdgTransport::new(path.to_string_lossy(), 5060);

        let first = t.prepare().unwrap();
        // Removing the file proves the second call did not re-read it, i.e.
        // one attachment rather than two.
        std::fs::remove_file(&path).ok();
        let second = t.prepare().unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn epdg_handles_ipv6_pcscf() {
        let path = temp_pcscf_file("2402:8100::1\n");
        let mut t = EpdgTransport::new(path.to_string_lossy(), 5060);

        let handle = t.prepare().unwrap();

        assert_eq!(handle.pcscf, "[2402:8100::1]:5060".parse().unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_pcscf_file_fails_at_the_discovery_stage() {
        let mut t = EpdgTransport::new("/nonexistent/pcscf", 5060);

        let err = t.prepare().unwrap_err();

        assert_eq!(err.stage, TransportStage::DiscoveringPcscf);
        assert!(err.detail.contains("failed to read P-CSCF address"));
    }

    #[test]
    fn malformed_pcscf_file_fails_at_the_discovery_stage() {
        let path = temp_pcscf_file("not-an-address");
        let mut t = EpdgTransport::new(path.to_string_lossy(), 5060);

        let err = t.prepare().unwrap_err();

        assert_eq!(err.stage, TransportStage::DiscoveringPcscf);
        assert!(err.detail.contains("invalid P-CSCF address"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn teardown_is_safe_before_prepare_and_twice_after() {
        let path = temp_pcscf_file("10.1.2.3");
        let mut t = EpdgTransport::new(path.to_string_lossy(), 5060);

        t.teardown().unwrap(); // before any prepare
        t.prepare().unwrap();
        t.teardown().unwrap();
        t.teardown().unwrap(); // twice

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn teardown_after_failed_prepare_is_safe() {
        let mut t = EpdgTransport::new("/nonexistent/pcscf", 5060);

        assert!(t.prepare().is_err());
        t.teardown().unwrap();
    }

    #[test]
    fn transport_error_converts_to_bridge_error_preserving_the_stage() {
        let err: BridgeError = TransportError::attaching("no carrier").into();

        let text = err.to_string();
        assert!(text.contains("attaching"), "stage lost: {text}");
        assert!(text.contains("no carrier"), "detail lost: {text}");
    }
}
