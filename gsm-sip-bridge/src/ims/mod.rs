//! IMS-AKA SIP REGISTER — an alternate mode alongside the existing GSM->SIP
//! voice flow, for registering to a mobile operator's IMS core over a
//! VoWiFi/ePDG tunnel (see `docker/`) using the SIM inside the modem.
//!
//! ## Why this doesn't go through the existing PJSIP-backed `SipBridge`
//!
//! IMS-AKA (RFC 3310) authenticates SIP REGISTER using the AKA `RES` value
//! (computed by the real SIM in response to a RAND/AUTN challenge) as the
//! digest "password" — a fundamentally different credential source than the
//! plain username/password `SipBridge`/`pjsua-safe::Account` supports today.
//! PJSIP does define an extensibility hook for this (`pjsip_cred_info.ext.aka`
//! / `pjsip_cred_cb`), but the system `libpjproject` this project links
//! against is compiled with `PJSIP_HAS_DIGEST_AKA_AUTH=0`, so that hook is
//! entirely absent from the linked library — using it would mean vendoring
//! and patch-rebuilding PJSIP itself. `pjsua-safe::Account::register` also
//! has no parameter for a callback or a pre-computed response, and offers no
//! way to intercept a 401 before PJSIP auto-responds to it.
//!
//! Since the actual protocol exchange is small (REGISTER -> 401 -> REGISTER
//! with an `Authorization` header) and this project already has everything
//! else needed (AT+CSIM access to the SIM via `modules::usim`, and now RFC
//! 2617/3310 digest math in `ims::digest`), this module handles the SIP
//! request/response transaction directly instead.

pub mod agent;
mod amr_rtp;
pub mod call;
/// `pub` rather than `pub(crate)`: `sip::server`'s registrar verifies inbound
/// REGISTER credentials with the same RFC 2617 math this module computes
/// outbound IMS-AKA responses with — a second MD5 digest implementation in the
/// same binary is exactly the duplication the constitution's simplicity
/// principle targets (spec 024, research.md R-007). Widened again to `pub`
/// for the `siptest` crate (specs/037-siptest-softphone, research.md R1),
/// which needs the same digest math to REGISTER as a plain handset — the
/// same rationale, one crate further out.
pub mod digest;
pub mod echo;
mod gm_ipsec;
pub mod lifecycle;
pub mod media_stats;
pub mod observability;
mod rtcp;
/// `pub` rather than private: `siptest` (specs/037-siptest-softphone,
/// research.md R1) reuses RTP framing, the μ-law codec and the WAV writer
/// rather than re-implementing them — the reasoning `digest` and
/// `sip_client` below already carry, extended one module further.
pub mod rtp;
pub mod sdp;
pub mod session;
/// `pub` rather than `pub(crate)`: `sip::server`'s registrar reuses this
/// module's request parser and UAS response builder to serve IP phones (spec
/// 024, research.md R-007). Widened again to `pub` for `siptest`
/// (specs/037-siptest-softphone, research.md R1), which builds a plain RFC
/// 3261 handset on the same message model and UAS response builders — see
/// `digest` above for the same rationale one crate further out. The
/// IMS-specific builders (`build_register`, `build_invite`) are visible too,
/// but `siptest` deliberately does not call them; its own REGISTER/INVITE are
/// plain RFC 3261, not IMS.
pub mod sip_client;
pub(crate) mod sms_pdu;
mod transcode;
pub mod transport;

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::AtCommander;
use crate::modules::pcsc_card::PcscTransport;
use crate::modules::usim::{self, AkaResult, ApduTransport};
use gm_ipsec::GmEndpoints;
use sip_client::{
    build_options, build_register, extract_challenge, format_sip_addr, parse_digest_challenge,
    random_hex, OptionsRequest, RegisterRequest, SipTransport,
};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

pub const DEFAULT_EXPIRES: u32 = 3600;
/// RFC 3310 §4.4: on a sync failure the client re-sends with an empty
/// password and an `auts` parameter; the server then issues a fresh
/// challenge. Cap resync attempts so a persistently out-of-sync SIM (or a
/// server that never accepts the resync) can't loop forever.
const MAX_RESYNC_ATTEMPTS: u32 = 2;

/// `P-Access-Network-Info` value for the ePDG/VoWiFi access leg. This is the
/// value the registration path hard-coded before the field was configurable,
/// so it remains the VoWiFi default (FR-019: no behavioural change).
pub const ACCESS_NETWORK_WLAN: &str = "3GPP-WLAN";

/// Every method this UAS's `dispatch_loop` actually serves — the single
/// source of truth `agent::mod`'s own `ALLOW` re-exports, and what
/// `sip_client::build_register`'s `Allow` header is built from, so a
/// REGISTER can no longer claim more than the dispatch loop delivers
/// (specs/041 conformance review, MT-10). Lives here, one level up from
/// both, because `agent::mod` (which owns the dispatch loop this describes)
/// already imports from `sip_client` (which needs this to build REGISTER) —
/// putting it in either would make the other reach across a boundary it
/// doesn't otherwise cross.
pub(crate) const UAS_ALLOW: &str = "INVITE, ACK, CANCEL, BYE, OPTIONS, MESSAGE, NOTIFY";

pub struct ImsRegisterConfig {
    pub modem_port: PathBuf,
    /// This line's SIM comes from a physical PC/SC reader
    /// (specs/023-omnikey-pcsc-vowifi), not the modem at `modem_port` —
    /// `register_session` connects to it via `PcscTransport` instead of
    /// opening `modem_port` over AT+CSIM. `imsi`/`imei` must both be `Some`
    /// in that case; there is no modem to fall back to `AT+CIMI`/`AT+CGSN`.
    pub pcsc_reader: bool,
    pub pcscf_addr: IpAddr,
    pub pcscf_port: u16,
    pub mcc: String,
    pub mnc: String,
    /// Overrides the IMSI read from the SIM via AT+CIMI, if set.
    pub imsi: Option<String>,
    /// Overrides the IMEI read from the modem via AT+CGSN, if set. Sent as
    /// the Contact header's `+sip.instance` — see `sip_client::RegisterRequest::imei`.
    pub imei: Option<String>,
    pub use_tcp: bool,
    /// Advertise `Supported: sec-agree` and a `Security-Client: ipsec-3gpp`
    /// proposal (RFC 3329 / TS 24.229 Annex H) on every REGISTER. Some
    /// networks (confirmed: Vodafone India's P-CSCF, which rejects a plain
    /// digest REGISTER with `421 Extension Required` / `Require: sec-agree`)
    /// require this before accepting REGISTER at all. This does **not**
    /// implement the actual Gm IPsec SA (no kernel XFRM/ESP setup) — it only
    /// tests whether the network will proceed on the strength of the header
    /// proposal alone, in case it's lenient about actually enforcing it.
    pub sec_agree: bool,
    /// Use this MSISDN (E.164) as the Public User Identity in
    /// To/From/Contact instead of the IMSI-derived temporary IMPU. The
    /// Authorization header's username (IMPI) is unaffected — see the CLI
    /// help text in `cli.rs` for the rationale.
    pub msisdn: Option<String>,
    /// Value for the `P-Access-Network-Info` header, describing the access
    /// network this registration is arriving over (TS 24.229 §7.2A.4).
    ///
    /// `ACCESS_NETWORK_WLAN` for the ePDG/VoWiFi path — which is what every
    /// caller sent before this field existed, so it is what they still send.
    /// The LTE path supplies an E-UTRAN value instead; see
    /// `crate::volte::pani::access_network_info`.
    pub access_network_info: String,
    /// Put the home-network domain in the REGISTER request line instead of the
    /// literal P-CSCF address. See `register_session`'s `request_uri`.
    pub register_uri_home_domain: bool,
    /// Pin the Gm IPsec auth / cipher algorithm instead of taking the
    /// network's highest-`q` offer. `None` = follow the offered preference.
    /// See `gm_ipsec::select_security_server`.
    pub gm_auth_alg: Option<String>,
    pub gm_cipher_alg: Option<String>,
}

#[derive(Debug)]
pub enum RegisterOutcome {
    Success {
        status: u16,
        headers: Vec<(String, String)>,
    },
    Rejected {
        status: u16,
        reason: String,
    },
}

/// A REGISTER transaction's outcome plus everything needed to send further
/// requests (e.g. INVITE, in `ims::call`) over the *same* session — reusing
/// the live transport (which, once Gm IPsec is set up, is the *only* place
/// the negotiated XFRM policy's selector matches) rather than reconnecting.
pub(crate) struct RegisteredSession {
    /// `None` only transiently inside `reconnect_transport` — the old
    /// socket must be actually dropped (not just marked for an abortive
    /// close) before the replacement can rebind its exact local port, so
    /// there is no valid `SipTransport` to hold in between. See
    /// `transport()`/`transport_mut()`.
    transport: Option<SipTransport>,
    realm: String,
    public_uri: String,
    local_addr: SocketAddr,
    /// The address the network reaches us on — our Gm protected *server*
    /// port (`port-s`), not `local_addr` (the *client* port we send from).
    /// Every `Contact` we advertise must carry this, or nothing
    /// network-initiated can ever be delivered. See
    /// `sip_client::RegisterRequest::contact_addr`.
    contact_addr: SocketAddr,
    use_tcp: bool,
    /// Next `CSeq` to use for a request on this session (already advanced
    /// past whatever REGISTER used).
    cseq: u32,
    gm_state: Option<(GmEndpoints, SaProposal, gm_ipsec::SecurityServerParams)>,
    xfrm_proto: &'static str,
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
    call_id: String,
    from_tag: String,
    pcscf_addr: SocketAddr,
    imei: String,
}

impl RegisteredSession {
    /// The lifetime the registrar granted this binding, in seconds.
    ///
    /// Falls back to `requested` when the response says nothing, which keeps
    /// behaviour identical on the networks in use today — every current
    /// carrier grants the hour that was asked for.
    pub(crate) fn granted_expires(&self, requested: u32) -> u32 {
        granted_expires(&self.headers, requested)
    }

    /// The identity to originate requests from, as a bare `user@host` (every
    /// caller wraps it as `sip:{}`).
    ///
    /// TS 24.229 requires an originating request to use a **registered public
    /// user identity**. We register with the IMSI-derived identity, which is
    /// legitimate for REGISTER but is not one of the IMPUs the network
    /// actually provisions for calls. Jio accepts it at registration and then
    /// fails anything it originates with `Q.850;cause=41 "temporary
    /// failure"`, routing the caller to an MSML announcement server for ~13s
    /// before answering `480` — a rejection that looks nothing like an
    /// identity problem (measured 2026-08-14).
    ///
    /// The registrar tells us the right identities in `P-Associated-URI` on
    /// the REGISTER `200 OK`; prefer the first `sip:` one. Falls back to the
    /// registration identity for networks that return none, which is what
    /// every carrier before this one did.
    pub(crate) fn origination_identity(&self) -> String {
        self.default_impu()
            .map(|impu| impu.trim_start_matches("sip:").to_string())
            .unwrap_or_else(|| self.public_uri.clone())
    }

    /// First `sip:` `P-Associated-URI` from the REGISTER `200 OK`, scheme
    /// included (`sip:+1555…@ims…`). `None` if the registrar returned none.
    pub(crate) fn default_impu(&self) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, v)| k.eq_ignore_ascii_case("P-Associated-URI") && v.contains("sip:"))
            .and_then(|(_, v)| {
                let start = v.find('<')? + 1;
                let end = v.find('>')?;
                Some(v[start..end].to_string())
            })
    }

    /// Our Gm **protected server port** (`port-s`) endpoint — where the
    /// network opens connections to deliver everything it originates
    /// (reg-event `NOTIFY`s, mobile-terminating `INVITE`s). `None` when the
    /// registration negotiated no Gm SA at all (`--sec-agree` off), in which
    /// case there is no such port and nothing can be delivered to us.
    /// See `sip_client::spawn_gm_server`.
    fn gm_server_addr(&self) -> Option<SocketAddr> {
        self.gm_state.as_ref().map(|(e, _, _)| e.local_s)
    }

    /// The live client transport. `Err` only in the brief window inside
    /// `reconnect_transport`, or lastingly if a Gm-protected rebind there
    /// failed and was left failed rather than falling back to an unprotected
    /// connection — every caller here already treats that like any other
    /// transport failure (log and move on, or propagate via `?`).
    pub(crate) fn transport(&self) -> BridgeResult<&SipTransport> {
        self.transport
            .as_ref()
            .ok_or_else(|| BridgeError::Ims("client transport is not connected".into()))
    }

    pub(crate) fn transport_mut(&mut self) -> BridgeResult<&mut SipTransport> {
        self.transport
            .as_mut()
            .ok_or_else(|| BridgeError::Ims("client transport is not connected".into()))
    }

    /// Tear down any installed Gm IPsec state — a one-shot diagnostic CLI
    /// isn't a persistent registration, so kernel XFRM state would
    /// otherwise leak across repeated invocations.
    fn cleanup(&mut self) {
        if let Some((endpoints, p, theirs)) = self.gm_state.take() {
            gm_ipsec::remove_gm_sas(&endpoints, &p, &theirs, self.xfrm_proto);
        }
    }

    /// Re-establishes the client transport (the connection the initial
    /// REGISTER went out on) after it dies mid-registration — e.g. a NAT or
    /// the P-CSCF itself silently drops an idle TCP connection during a long
    /// call, where no SIP traffic crosses this leg until the closing `BYE`
    /// (media is a separate RTP path). Rebinds to the exact `port-c` local
    /// port and reconnects to the exact Gm-protected `remote_s` peer the
    /// original registration negotiated — the same recipe `register_session`
    /// uses right after installing the Gm SAs — so the still-live IPsec SA
    /// (its lifetime is independent of any one TCP connection) still applies
    /// to the new socket.
    ///
    /// Drops the old socket (after an abortive `force_close`) *before*
    /// attempting the rebind: the replacement reuses the exact same (local
    /// port, remote peer) pair, and TCP will not let two live sockets share
    /// one 4-tuple — `SO_REUSEADDR` only helps past `TIME_WAIT`, not a peer
    /// that is still open. This is why `transport` is briefly `None` here,
    /// and why the field is an `Option` at all.
    ///
    /// Connects plainly to `pcscf_addr` only if there is no Gm SA to reuse
    /// in the first place (`--sec-agree` off). If a Gm SA *is* active and
    /// the protected rebind itself fails, this returns `Err` rather than
    /// falling back to a plain connection — unlike `register_session`'s
    /// equivalent step, where that fallback is sound because the network
    /// hasn't committed to requiring protection yet. Once a Gm SA is
    /// active, a plain reconnect wouldn't match the installed XFRM policy's
    /// selector, so the P-CSCF would reject or silently drop whatever went
    /// out on it — worse than a clean, visible failure. `transport` stays
    /// `None` in that case; every caller already treats that identically to
    /// any other transport error, and the next scheduled renewal replaces
    /// the whole session (and its transport) with a fresh one anyway.
    ///
    /// Does not touch `inbound`'s reader threads — the caller must restart
    /// the client-reader half (`session::restart_client_reader`) once this
    /// returns `Ok`; the Gm protected-server-port listener is untouched by
    /// any of this; it is an independent socket the client transport dying
    /// does not affect.
    pub(crate) fn reconnect_transport(&mut self) -> BridgeResult<()> {
        if let Some(old) = self.transport.take() {
            old.force_close();
        }
        // No plain-connect fallback here if the Gm-protected rebind fails,
        // unlike `register_session`'s equivalent step: that fallback is only
        // sound *before* the network has committed to requiring protection
        // for this registration. Once a Gm SA is active, the P-CSCF expects
        // further requests protected under it — a plain reconnect to
        // `pcscf_addr` wouldn't match the installed XFRM policy's selector,
        // so it would either go out unprotected (a downgrade the network may
        // reject or silently drop) or simply not be delivered as intended.
        // Leaving `transport` `None` and propagating the error is honest:
        // every caller already treats it exactly like any other transport
        // failure, and the next scheduled renewal negotiates a fresh SA and
        // replaces this session (and its transport) wholesale.
        let new_transport = match self.gm_state.as_ref() {
            Some((endpoints, _, _)) => SipTransport::connect_from(
                endpoints.local_c.port(),
                endpoints.remote_s,
                self.use_tcp,
            )?,
            None => SipTransport::connect(self.pcscf_addr, self.use_tcp)?,
        };
        self.local_addr = new_transport.local_addr()?;
        self.transport = Some(new_transport);
        Ok(())
    }

    /// Send an UNREGISTER to the server (a REGISTER with Expires: 0) to
    /// clear all contacts. Best-effort only: transport/timeout errors are
    /// logged and ignored, since a registration that cannot unregister
    /// cleanly is better than one that litters kernel XFRM state, and a
    /// crashed registrar has already timed out the old registration anyway.
    ///
    /// **Note on response validation**: UNREGISTER is sent without
    /// re-authentication (the Digest nonce would have expired). Most SIP
    /// registrars accept this for established registrations (matched by
    /// Call-ID/From), but some stricter implementations may reject it. In
    /// either case, the kernel XFRM state is torn down immediately after,
    /// so accepting both 2xx and error responses is appropriate here.
    ///
    /// **Note on cleanup delay**: If the registrar is unreachable,
    /// send_and_recv will block for ~5 seconds waiting for the timeout.
    /// This is acceptable during shutdown but should be monitored if called
    /// in a hot path.
    pub(crate) fn unregister(&mut self) {
        let via_transport = if self.use_tcp { "TCP" } else { "UDP" };
        let request_uri = format_sip_addr(self.pcscf_addr);

        self.cseq = self.cseq.wrapping_add(1);
        let branch = format!("z9hG4bK{}", random_hex(6));
        let unreg = build_register(&RegisterRequest {
            registrar_uri: &request_uri,
            public_uri: &self.public_uri,
            local_addr: self.local_addr,
            contact_addr: self.contact_addr,
            call_id: &self.call_id,
            from_tag: &self.from_tag,
            branch: &branch,
            cseq: self.cseq,
            expires: 0,
            transport: via_transport,
            authorization: None,
            extra_headers: &[],
            imei: &self.imei,
        });

        let Ok(transport) = self.transport_mut() else {
            tracing::debug!("cannot send UNREGISTER: client transport is not connected");
            return;
        };
        match transport.send_and_recv(&unreg) {
            Ok(resp) => {
                if (200..300).contains(&resp.status) {
                    tracing::debug!("sent UNREGISTER, server accepted");
                } else {
                    tracing::debug!(status = resp.status, reason = %resp.reason, "sent UNREGISTER, server rejected (registration will expire naturally)");
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "UNREGISTER send failed (registration will expire naturally)");
            }
        }
    }

    /// Send an out-of-dialog `OPTIONS` keepalive on the client connection and
    /// return the `CSeq` it went out with, so the caller can correlate the
    /// response back to it.
    ///
    /// **Fire-and-forget**: this uses `send`, not `send_and_recv`. The reader
    /// thread (`session::spawn_client_reader`) owns the read half of this
    /// socket; a second reader here would race it and corrupt SIP framing. The
    /// response is delivered through that thread to the dispatch loop's
    /// response arm, which matches it by `CSeq`. See specs/028-gm-tcp-reconnect
    /// R1 — this constraint is the whole reason the probe is asynchronous.
    pub(crate) fn send_gm_ping(&mut self) -> BridgeResult<u32> {
        let via_transport = if self.use_tcp { "TCP" } else { "UDP" };
        let request_uri = format_sip_addr(self.pcscf_addr);
        self.cseq = self.cseq.wrapping_add(1);
        let cseq = self.cseq;
        let branch = format!("z9hG4bK{}", random_hex(6));
        let options = build_options(&OptionsRequest {
            request_uri: &request_uri,
            local_addr: self.local_addr,
            transport: via_transport,
            public_uri: &self.public_uri,
            from_tag: &self.from_tag,
            call_id: &self.call_id,
            cseq,
            branch: &branch,
        });
        self.transport_mut()?.send(&options)?;
        Ok(cseq)
    }
}

/// Current lifecycle state of a *persistent* IMS-AKA registration
/// (`specs/011-vowifi-sip-bridge` User Story 2's Agent A, which keeps
/// re-registering indefinitely) — distinct from `RegisterOutcome`, which
/// only reports a one-shot CLI transaction's final result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationState {
    Unregistered,
    Registering,
    Registered,
    Renewing,
    Failed,
}

/// Health of a registered line's Gm signaling connection, independent of the
/// registration state above. A registration can read `Registered` while its
/// Gm connection is silently dead — the exact failure
/// specs/028-gm-tcp-reconnect addresses.
///
/// `Up` is the default (not an `Unknown` variant): a registration that just
/// completed is itself a successful round trip, so treating a fresh line as
/// unknown would report a false degradation on every startup. `Failed` is
/// **not** terminal — the loop keeps retrying on backoff so a line can still
/// self-heal when the network recovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GmConnectionState {
    /// The last liveness probe round-tripped, including the confirming probe
    /// after a reconnect.
    Up,
    /// A drop was detected; repair is in progress. `attempts` counts
    /// *consecutive* failures, reset to zero on any confirmed recovery.
    Reconnecting {
        since: std::time::SystemTime,
        attempts: u32,
    },
    /// Repair has been escalated to re-registration and that is also failing.
    Failed { since: std::time::SystemTime },
}

impl GmConnectionState {
    /// Whether the connection is currently healthy — the input `can_answer`
    /// and the metrics gauge both read.
    pub fn is_up(&self) -> bool {
        matches!(self, GmConnectionState::Up)
    }

    /// Wire/CLI rendering: `up`, `reconnecting since <ts> (attempt N)`, or
    /// `failed since <ts>`. Timestamps are RFC 3339 in UTC.
    pub fn render(&self) -> String {
        fn ts(t: std::time::SystemTime) -> String {
            chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()
        }
        match self {
            GmConnectionState::Up => "up".to_string(),
            GmConnectionState::Reconnecting { since, attempts } => {
                format!("reconnecting since {} (attempt {})", ts(*since), attempts)
            }
            GmConnectionState::Failed { since } => {
                format!("failed since {}", ts(*since))
            }
        }
    }
}

/// Snapshot of Agent A's registration health, per
/// `specs/011-vowifi-sip-bridge/data-model.md`'s "VoWiFi Line Registration"
/// entity — what FR-008/User Story 3's status reporting needs.
#[derive(Debug, Clone)]
pub struct RegistrationStatus {
    pub state: RegistrationState,
    pub registered_at: Option<std::time::SystemTime>,
    pub expires_at: Option<std::time::SystemTime>,
    pub last_failure: Option<(std::time::SystemTime, String)>,
    // The health inputs the dispatch loop keeps current so the status listener
    // can answer `can_answer`/`blocked_reason` (`ims::lifecycle::ServiceHealth`)
    // without touching the modem itself. In-memory only — `render_status`
    // persists just the four fields above, so these carry sensible defaults on
    // a status read from disk.
    /// Whether the network attachment underneath the registration is up. The
    /// Wi-Fi path has no such attachment, so it is left `true` (its default)
    /// and health then turns only on `registered`/`busy`.
    pub attached: bool,
    /// Whether a call is in progress.
    pub busy: bool,
    /// Whether the telephone-side half holds the PBX registration the outbound
    /// bridge leg needs. Left `true` on the Wi-Fi path (not tracked there).
    pub pbx_registered: bool,
    /// Maintenance currently held back for a call, if any — reported so a
    /// deferral reads as deliberate rather than as a stall.
    pub deferred_maintenance: Option<crate::ims::lifecycle::Maintenance>,
    /// Health of the Gm signaling connection underneath the registration.
    /// In-memory only, like the health inputs above — `render_status`
    /// persists only the first four fields, so a status read from disk
    /// carries the `Up` default. See [`GmConnectionState`].
    pub gm_connection: GmConnectionState,
}

impl Default for RegistrationStatus {
    fn default() -> Self {
        Self {
            state: RegistrationState::Unregistered,
            registered_at: None,
            expires_at: None,
            last_failure: None,
            attached: true,
            busy: false,
            pbx_registered: true,
            deferred_maintenance: None,
            gm_connection: GmConnectionState::Up,
        }
    }
}

impl RegistrationStatus {
    /// The [`ServiceHealth`](crate::ims::lifecycle::ServiceHealth) this snapshot
    /// implies — the single derivation the status listener answers a
    /// `can_answer`/`blocked_reason` query from, so what a `volte-status` caller
    /// reads agrees by construction with the admission the dispatch loop
    /// applies. `registered` is *only* the `Registered` state: `Renewing` or
    /// `Failed` cannot answer, which is the honest answer even mid-renewal.
    pub fn health(&self) -> crate::ims::lifecycle::ServiceHealth {
        self.health_at(std::time::SystemTime::now())
    }

    /// [`Self::health`] with the clock injected, so expiry is testable without
    /// waiting an hour.
    ///
    /// This is where `expires_at` finally gets *consulted*. It was recorded
    /// from the first release and read by nothing: the 2026-08-16 status output
    /// printed an expiry two and three-quarter hours in the past directly above
    /// `can_answer: true`, because no code compared the two.
    pub fn health_at(&self, now: std::time::SystemTime) -> crate::ims::lifecycle::ServiceHealth {
        crate::ims::lifecycle::ServiceHealth {
            registered: self.state == RegistrationState::Registered,
            registration_expired: self.expires_at.is_some_and(|e| now >= e),
            attached: self.attached,
            pbx_registered: self.pbx_registered,
            gm_connection_up: self.gm_connection.is_up(),
            busy: self.busy,
            deferred: self.deferred_maintenance,
        }
    }
}

/// Whether a registration expiring at `expires_at` should be renewed *now*,
/// given the current time and how much headroom to leave before the actual
/// expiry — renewing early leaves margin for the renewal's own network
/// round-trip and AKA challenge to finish before the old registration would
/// actually lapse (FR-001/FR-007: no gap in which an inbound call would go
/// unanswered). Pure and clock-injectable so it's testable without waiting
/// on a real timer.
/// The registration lifetime the network actually granted.
///
/// A registrar may grant less than was requested, and renewing on the requested
/// value would then leave a window where the binding has lapsed but we still
/// believe it is live. Prefers the `Expires` header, falls back to `Contact`'s
/// `expires=` parameter, then to what was asked for.
pub fn granted_expires(headers: &[(String, String)], requested: u32) -> u32 {
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("expires") {
            if let Ok(v) = value.trim().parse::<u32>() {
                return v;
            }
        }
    }
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("contact") {
            if let Some(rest) = value.to_ascii_lowercase().find("expires=").map(|i| i + 8) {
                let tail: String = value[rest..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(v) = tail.parse::<u32>() {
                    return v;
                }
            }
        }
    }
    requested
}

/// How far before expiry to renew, for a registration of `granted` seconds.
///
/// Honouring a short granted lifetime is only safe together with this. The
/// headroom used to be a flat 300s against an assumed 3600s lifetime; the
/// moment a registrar grants less than twice the headroom, `renewal_due`
/// becomes permanently true and the agent re-registers on *every idle poll* —
/// once per second — for as long as the line is up. Scaling the margin to the
/// lifetime keeps a comfortable early renewal without that runaway.
///
/// Pure and clock-free, like `renewal_due`, so the schedule is testable.
pub fn renewal_headroom_for(
    granted: std::time::Duration,
    preferred: std::time::Duration,
) -> std::time::Duration {
    preferred.min(granted / 2)
}

pub fn renewal_due(
    now: std::time::SystemTime,
    expires_at: std::time::SystemTime,
    headroom: std::time::Duration,
) -> bool {
    match expires_at.checked_sub(headroom) {
        Some(renew_at) => now >= renew_at,
        // headroom >= time-to-expiry (or expires_at is implausibly close to
        // the epoch): nothing left to wait for, renew immediately.
        None => true,
    }
}

/// Run the IMS-AKA REGISTER flow to completion (one challenge/response
/// round, plus up to `MAX_RESYNC_ATTEMPTS` AKA resyncs) and report the
/// final SIP status.
pub fn run_register(cfg: &ImsRegisterConfig) -> BridgeResult<RegisterOutcome> {
    let mut session = register_session(cfg)?;
    session.cleanup();
    match session.status {
        200 => Ok(RegisterOutcome::Success {
            status: session.status,
            headers: session.headers,
        }),
        _ => Ok(RegisterOutcome::Rejected {
            status: session.status,
            reason: session.reason,
        }),
    }
}

/// How long to wait between attempts to open a modem line's serial port.
const MODEM_OPEN_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// How long to keep waiting before giving up on it.
///
/// Bounded, unlike Agent B's control-channel bind: the conflict this waits out
/// is one EAP-AKA exchange, which takes seconds. A genuinely wrong or missing
/// port must still surface as an error rather than hanging the agent forever.
///
/// `pub(crate)` so `ims::agent::watchdog`'s budget-derivation test can recompute
/// the renewal worst case from the real constants rather than a copy of them.
pub(crate) const MODEM_OPEN_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Opens the modem's serial port, waiting out a transient exclusive-open
/// conflict with `vowifi-usim-bridge`.
///
/// `serialport` opens exclusively, so only one of the two can hold the port at
/// a time — but neither holds it for long, which is why waiting is the whole
/// fix and nothing more elaborate is warranted:
///
/// - the usim bridge opens it lazily on vpcd Power On and **drops it on Power
///   Off** (`SessionState::Unpowered` owns no `AtCommander`), so charon has it
///   only across an EAP-AKA exchange;
/// - this transport is a local of `register_session` and is **not** stored in
///   `RegisteredSession`, so it is dropped when the REGISTER exchange returns —
///   seconds, once per registration refresh, not for the session's lifetime.
///
/// Verified on a live two-line deployment: with both lines registered and
/// stable, nothing at all held `/dev/ttyUSB0`. So the contention is a brief,
/// occasional overlap and a bounded wait resolves it completely.
///
/// This matters because it rules out the tempting redesign of routing IMS-AKA
/// through pcscd's vpcd slot the way charon does. There is no long-lived hold
/// to eliminate, and sharing one virtual card between charon's EAP and this
/// registration would introduce genuinely concurrent access to card state
/// (selected AID), requiring every SELECT+AUTHENTICATE to become transactional
/// — real risk in the one path that must work, for no gain.
///
/// Losing the race used to fail the agent outright, whereupon the supervisor
/// restarted it five seconds later: the entire IMS session torn down and
/// rebuilt over a conflict that clears on its own in seconds. Observed live as
/// `failed to open serial /dev/ttyUSB0: Unable to acquire exclusive lock`,
/// crash-looping until it happened to win a race.
///
/// Generic over open and sleep so the policy is testable without a serial port
/// or a real clock.
fn open_modem_waiting<T, E: std::fmt::Display>(
    port: &str,
    max_wait: std::time::Duration,
    interval: std::time::Duration,
    mut open: impl FnMut() -> Result<T, E>,
    mut sleep: impl FnMut(std::time::Duration),
) -> Result<T, E> {
    let mut waited = std::time::Duration::ZERO;
    loop {
        match open() {
            Ok(opened) => {
                if !waited.is_zero() {
                    tracing::info!(
                        port = %port,
                        waited_secs = waited.as_secs(),
                        "modem serial acquired after waiting out a conflicting holder"
                    );
                }
                return Ok(opened);
            }
            Err(e) => {
                if waited >= max_wait {
                    return Err(e);
                }
                if waited.is_zero() {
                    tracing::warn!(
                        port = %port,
                        error = %e,
                        "modem serial busy (vowifi-usim-bridge is probably mid-EAP); waiting"
                    );
                }
                sleep(interval);
                waited += interval;
            }
        }
    }
}

pub(crate) fn register_session(cfg: &ImsRegisterConfig) -> BridgeResult<RegisteredSession> {
    // A pcsc_reader line's SIM sits in a real PC/SC reader, not the modem at
    // `modem_port` (specs/023-omnikey-pcsc-vowifi). Only one `AtCommander` is
    // ever opened on `modem_port` per call — it must serve both the
    // AT+CIMI/AT+CGSN fallback reads below and the AUTHENTICATE/SELECT APDU
    // traffic later, since `serialport`'s exclusive-open means a second
    // handle on the same tty would fail outright.
    let mut card: Box<dyn ApduTransport>;
    let imsi: String;
    let imei: String;
    if cfg.pcsc_reader {
        // Config validation guarantees both are set for a pcsc_reader
        // line — there is no modem to fall back to AT+CIMI/AT+CGSN, and
        // discovery auto-generates an imei when none is overridden.
        imsi = cfg.imsi.clone().ok_or_else(|| {
            BridgeError::Ims("pcsc_reader line has no imsi_override configured".into())
        })?;
        imei = cfg.imei.clone().ok_or_else(|| {
            BridgeError::Ims("pcsc_reader line has no imei (expected one to be auto-generated at discovery time)".into())
        })?;
        // Matches against the card's own EF_IMSI rather than picking "the
        // first reader" — with more than one pcsc_reader line configured,
        // that would authenticate every line as whichever subscriber
        // happened to be plugged into the first reader pcscd lists.
        card = Box::new(PcscTransport::connect(&imsi)?);
    } else {
        let mut at = open_modem_waiting(
            &cfg.modem_port.to_string_lossy(),
            MODEM_OPEN_MAX_WAIT,
            MODEM_OPEN_RETRY_INTERVAL,
            || AtCommander::open(&cfg.modem_port),
            std::thread::sleep,
        )?;
        imsi = match &cfg.imsi {
            Some(imsi) => imsi.clone(),
            None => at.query_imsi()?,
        };
        imei = match &cfg.imei {
            Some(imei) => imei.clone(),
            None => at.query_imei()?,
        };
        card = Box::new(at);
    }
    tracing::info!(imsi = %imsi, "read IMSI from SIM");
    tracing::info!(imei = %imei, "read IMEI from modem");

    let aid = usim::discover_usim_aid(card.as_mut())?;
    usim::select_usim(card.as_mut(), &aid)?;
    tracing::info!(aid = %aid.iter().map(|b| format!("{b:02X}")).collect::<String>(), "selected USIM application");

    let realm = format!("ims.mnc{}.mcc{}.3gppnetwork.org", cfg.mnc, cfg.mcc);
    // The IMPI (private identity) — always IMSI-based per TS 33.203,
    // regardless of --msisdn. Used only for the Authorization header's
    // username and the digest HA1 computation, never for To/From/Contact.
    let impi_uri = format!("{imsi}@{realm}");
    // The Public User Identity used in To/From/Contact: either the
    // IMSI-derived temporary IMPU (default, works on Airtel) or an
    // MSISDN-based IMPU if --msisdn is given (testing whether a network's
    // HSS is pickier about binding a Contact to the private identity).
    let public_uri = match &cfg.msisdn {
        Some(msisdn) => format!("{msisdn}@{realm}"),
        None => impi_uri.clone(),
    };
    let pcscf: SocketAddr = SocketAddr::new(cfg.pcscf_addr, cfg.pcscf_port);
    let request_uri = register_request_uri(pcscf, &realm, cfg.register_uri_home_domain);

    let call_id = random_hex(8);
    let from_tag = random_hex(4);
    let mut cseq: u32 = 1;

    // Open the connection first so Via/Contact in even the *first* REGISTER
    // carry our real tunnel-assigned address rather than a placeholder —
    // some P-CSCFs silently drop a REGISTER with an unspecified Contact.
    // The same connection is reused for the challenge-response retry too.
    // `Option` so the Gm IPsec reconnect below can explicitly drop (close)
    // this connection before rebinding its exact local port for the new one
    // — SO_REUSEADDR alone doesn't help while the old socket is still open.
    let mut transport = Some(SipTransport::connect(pcscf, cfg.use_tcp)?);
    let mut local_addr = transport.as_ref().unwrap().local_addr()?;
    tracing::info!(local = %local_addr, peer = %pcscf, "connected to P-CSCF");
    let via_transport = if cfg.use_tcp { "TCP" } else { "UDP" };

    // Mandated by TS 24.229 so the P-CSCF can attribute the request to the
    // right access leg; real UEs and Asterisk's Gm transport both always send
    // this. The value must describe the access actually in use — `3GPP-WLAN`
    // over an ePDG tunnel, `3GPP-E-UTRAN-FDD` (with a cell id) over LTE — and
    // getting it wrong is a plausible reason for a P-CSCF to reject a
    // registration that is otherwise perfectly reachable.
    let mut extra_headers = vec![format!(
        "P-Access-Network-Info: {}",
        cfg.access_network_info
    )];
    // A plain `Supported: sec-agree` (advertising the capability) was not
    // enough on Airtel — captured wire traffic from a working Asterisk
    // registration shows it sends `Require`/`Proxy-Require: sec-agree`
    // (mandating the extension) plus `Supported: path, sec-agree`, and
    // already attaches an empty placeholder `Authorization` header on the
    // very first, pre-challenge REGISTER.
    let placeholder_auth = format!(
        "Digest uri=\"sip:{realm}\",username=\"{impi_uri}\",response=\"\",realm=\"{realm}\",nonce=\"\""
    );
    let mut proposal: Option<SaProposal> = None;
    if cfg.sec_agree {
        extra_headers.push("Require: sec-agree".to_string());
        extra_headers.push("Proxy-Require: sec-agree".to_string());
        extra_headers.push("Supported: path, sec-agree".to_string());
        let p = SaProposal {
            spi_c: rand::random::<u32>() | 0x1,
            spi_s: rand::random::<u32>() | 0x1,
            port_c: local_addr.port(),
            port_s: local_addr.port().wrapping_add(2),
        };
        extra_headers.extend(build_security_client_headers(&p));
        proposal = Some(p);
    }
    // The port the network will deliver to. Chosen up front (it is part of
    // the Security-Client proposal) so even the first, pre-IPsec REGISTER
    // advertises it, exactly as a real UE does.
    let mut contact_addr = match &proposal {
        Some(p) => SocketAddr::new(local_addr.ip(), p.port_s),
        None => local_addr,
    };
    let xfrm_proto = if cfg.use_tcp { "tcp" } else { "udp" };
    // Populated once Gm IPsec SAs are installed, so they can be torn down
    // before this function returns rather than leaking kernel XFRM state
    // across repeated `ims-register` invocations.
    let mut gm_state: Option<(GmEndpoints, SaProposal, gm_ipsec::SecurityServerParams)> = None;

    // First REGISTER — no credentials; expect a 401 challenge.
    let branch = format!("z9hG4bK{}", random_hex(6));
    let initial = build_register(&RegisterRequest {
        registrar_uri: &request_uri,
        public_uri: &public_uri,
        local_addr,
        contact_addr,
        call_id: &call_id,
        from_tag: &from_tag,
        branch: &branch,
        cseq,
        expires: DEFAULT_EXPIRES,
        transport: via_transport,
        authorization: if cfg.sec_agree {
            Some(&placeholder_auth)
        } else {
            None
        },
        extra_headers: &extra_headers,
        imei: &imei,
    });

    let mut resp = transport.as_mut().unwrap().send_and_recv(&initial)?;
    tracing::info!(status = resp.status, reason = %resp.reason, "initial REGISTER response");
    if let Some(sec_server) = resp.header("Security-Server") {
        tracing::info!(security_server = %sec_server, "network proposed Gm IPsec parameters");
    }

    let mut resync_attempts = 0;
    loop {
        if resp.status != 401 {
            break;
        }
        let www_auth = resp
            .header("WWW-Authenticate")
            .ok_or_else(|| BridgeError::Ims("401 with no WWW-Authenticate header".into()))?
            .to_string();
        let params = parse_digest_challenge(&www_auth)?;
        let challenge = extract_challenge(&params)?;
        if challenge.algorithm.as_deref() != Some("AKAv1-MD5") {
            tracing::warn!(
                algorithm = ?challenge.algorithm,
                "challenge algorithm is not AKAv1-MD5 — RES-as-password digest math will not apply"
            );
        }

        let nonce_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &challenge.nonce)
                .map_err(|e| BridgeError::Ims(format!("nonce is not valid base64: {e}")))?;
        if nonce_bytes.len() < 32 {
            return Err(BridgeError::Ims(format!(
                "nonce too short for RAND+AUTN: {} bytes",
                nonce_bytes.len()
            )));
        }
        let mut rand_arr = [0u8; 16];
        let mut autn_arr = [0u8; 16];
        rand_arr.copy_from_slice(&nonce_bytes[0..16]);
        autn_arr.copy_from_slice(&nonce_bytes[16..32]);

        let aka = usim::authenticate(card.as_mut(), &rand_arr, &autn_arr)?;

        cseq += 1;
        // RFC 2617 requires this to match the Request-URI of the message it's
        // attached to (it feeds into the HA2 digest and the server checks it).
        let uri = format!("sip:{request_uri}");
        // Every offer, not just the first: the P-CSCF sends one header per
        // algorithm combination it accepts, and which one we commit to decides
        // whether its ESP replies will authenticate at all.
        let sec_selected = {
            let offers = resp.headers_all("Security-Server");
            if offers.is_empty() {
                None
            } else {
                match gm_ipsec::select_security_server(
                    &offers,
                    cfg.gm_auth_alg.as_deref(),
                    cfg.gm_cipher_alg.as_deref(),
                ) {
                    Ok((params, _selected_raw)) => {
                        tracing::info!(
                            alg = %params.alg,
                            ealg = %params.ealg,
                            q = params.q,
                            offered = offers.len(),
                            "selected Gm IPsec algorithm"
                        );
                        // The echo is the *whole* Security-Server list, not
                        // just the mechanism we committed to. Security-Verify
                        // is RFC 3329's downgrade check: the P-CSCF compares
                        // what we send back against everything it offered, so
                        // a partial echo reads as a tampered negotiation.
                        // Measured on Jio 2026-08-14 — it offers five
                        // mechanisms, we echoed the one we picked, and every
                        // protected REGISTER came back `494 Security
                        // Agreement Required`.
                        Some((params, offers.join(", ")))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "no usable Security-Server offer");
                        None
                    }
                }
            }
        };
        let (auth_header, was_resync) = match aka {
            AkaResult::Success { res, ck, ik } => {
                tracing::info!("AKA success, building Authorization response");
                if let (Some(p), Some((theirs, sec_verify))) = (proposal.as_ref(), sec_selected) {
                    {
                        {
                            let endpoints =
                                GmEndpoints::new(local_addr.ip(), pcscf.ip(), p, &theirs);
                            match gm_ipsec::install_gm_sas(
                                &endpoints, p, &theirs, xfrm_proto, &ik, &ck,
                            ) {
                                Ok(()) => {
                                    tracing::info!("Gm IPsec SAs installed");
                                    let new_dst = SocketAddr::new(pcscf.ip(), theirs.port_s);
                                    // Must close the existing plaintext connection before
                                    // rebinding its exact local port (our proposed port-c)
                                    // for the Gm-protected one — SO_REUSEADDR alone doesn't
                                    // let a new socket claim a port an open one still holds.
                                    if let Some(t) = transport.as_ref() {
                                        t.force_close();
                                    }
                                    drop(transport.take());
                                    match SipTransport::connect_from(p.port_c, new_dst, cfg.use_tcp)
                                    {
                                        Ok(new_transport) => {
                                            local_addr = new_transport.local_addr()?;
                                            contact_addr = endpoints.local_s;
                                            transport = Some(new_transport);
                                            tracing::info!(local = %local_addr, peer = %new_dst, "reconnected over Gm IPsec transport");
                                            // RFC 3329 §2.4: echo the network's own
                                            // Security-Server value back in a Security-Verify
                                            // header on the request sent over the now-selected
                                            // SA, confirming which negotiated association is in
                                            // use (a captured working Asterisk registration
                                            // always includes this on the post-IPsec retry).
                                            extra_headers
                                                .push(format!("Security-Verify: {sec_verify}"));
                                            gm_state = Some((endpoints, p.clone(), theirs));
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "failed to reconnect over the negotiated Gm port; reopening the original connection");
                                            transport =
                                                Some(SipTransport::connect(pcscf, cfg.use_tcp)?);
                                            local_addr =
                                                transport.as_ref().unwrap().local_addr()?;
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "failed to install Gm IPsec SAs; resending on the original transport")
                                }
                            }
                        }
                    }
                }
                (
                    build_authorization(&impi_uri, &challenge, &uri, &res),
                    false,
                )
            }
            AkaResult::SyncFailure { auts } => {
                resync_attempts += 1;
                if resync_attempts > MAX_RESYNC_ATTEMPTS {
                    return Err(BridgeError::Ims(
                        "AKA sync failure persisted past max resync attempts".into(),
                    ));
                }
                tracing::warn!(
                    attempt = resync_attempts,
                    "AKA sync failure, sending AUTS resync"
                );
                (
                    build_resync_authorization(&impi_uri, &challenge, &uri, &auts),
                    true,
                )
            }
        };

        let branch = format!("z9hG4bK{}", random_hex(6));
        let retry = build_register(&RegisterRequest {
            registrar_uri: &request_uri,
            public_uri: &public_uri,
            local_addr,
            contact_addr,
            call_id: &call_id,
            from_tag: &from_tag,
            branch: &branch,
            cseq,
            expires: DEFAULT_EXPIRES,
            transport: via_transport,
            authorization: Some(&auth_header),
            extra_headers: &extra_headers,
            imei: &imei,
        });

        let next_resp = transport.as_mut().unwrap().send_and_recv(&retry)?;
        tracing::info!(status = next_resp.status, reason = %next_resp.reason, "REGISTER response");
        if let Some(sec_server) = next_resp.header("Security-Server") {
            tracing::info!(security_server = %sec_server, "network proposed Gm IPsec parameters");
        }
        resp = next_resp;

        // A 401 after a resync-only send (empty-password + auts) means the
        // server accepted the resync and issued a fresh challenge — loop
        // again, this time with real AKA credentials. A 401 after a request
        // that carried real credentials means auth was rejected outright;
        // stop rather than looping forever.
        if resp.status == 401 && was_resync {
            continue;
        }
        break;
    }

    let transport = transport
        .take()
        .ok_or_else(|| BridgeError::Ims("transport unexpectedly absent after REGISTER".into()))?;

    Ok(RegisteredSession {
        transport: Some(transport),
        realm,
        public_uri,
        local_addr,
        contact_addr,
        use_tcp: cfg.use_tcp,
        cseq: cseq + 1,
        gm_state,
        xfrm_proto,
        status: resp.status,
        reason: resp.reason,
        headers: resp.headers,
        call_id,
        from_tag,
        pcscf_addr: pcscf,
        imei,
    })
}

/// Proposed identifiers for our end of the Gm IPsec SA pair, sent in the
/// `Security-Client` header (TS 24.229 Annex H profile of RFC 3329).
///
/// Two logical SAs are negotiated: one carrying UE->P-CSCF traffic (a port
/// and SPI on the *P-CSCF*, called `port-c`/`spi-c` in both parties'
/// headers) and one carrying P-CSCF->UE traffic (a port and SPI on the
/// *UE*, called `port-s`/`spi-s`). Each party only truly controls the
/// identifiers for traffic it receives — the P-CSCF's response is
/// authoritative for `port-c`/`spi-c` (a port on itself), while our own
/// `port-s`/`spi-s` (a port on us) stands as proposed unless the response
/// says otherwise.
#[derive(Clone)]
pub struct SaProposal {
    pub spi_c: u32,
    pub spi_s: u32,
    pub port_c: u16,
    pub port_s: u16,
}

/// The URI for the REGISTER request line (`REGISTER sip:<this> SIP/2.0`).
///
/// Which form belongs here is carrier-specific, and each one is rejected
/// outright by some network, so it cannot be a constant.
///
/// `home_domain == false` (the default): PJSIP-based implementations (e.g.
/// Asterisk's res_pjsip_outbound_registration, via pjsip_regc_init's
/// `srv_url`) use the literal P-CSCF address from `server_uri` rather than
/// the home-network realm, and matching that is what gets past Airtel's
/// registrar — a realm Request-URI there draws an instant `406 User
/// Unknown`.
///
/// `home_domain == true`: what TS 24.229 §5.1.1.2 actually mandates, and what
/// Jio requires. Its P-CSCF finds its own address in the request line, routes
/// the REGISTER back to itself and trips loop detection — `483 Too Many Hops`
/// from one instance and a blanket `403 Forbidden` from another, both arriving
/// before any challenge, and both indistinguishable at a glance from "this
/// subscriber is not provisioned for VoWiFi" (which is exactly how they were
/// first misread). Bisected live 2026-08-14 with a raw-REGISTER prober:
/// byte-identical REGISTERs differing only in the request line, where the
/// home-domain form draws the real `401` + AKA challenge and a populated
/// `Security-Server` list.
///
/// The digest `uri` parameter follows this value (RFC 2617 requires the two to
/// match, and the server checks it) — see `register_session`'s `uri`.
fn register_request_uri(pcscf: SocketAddr, realm: &str, home_domain: bool) -> String {
    if home_domain {
        realm.to_string()
    } else {
        format_sip_addr(pcscf)
    }
}

/// Build the `Supported: sec-agree` + `Security-Client: ipsec-3gpp` header
/// pair (RFC 3329 / TS 24.229 Annex H) that some networks require even to
/// get past an initial `421 Extension Required`.
///
/// The wire format here matches sysmocom's `volte.c` as actually captured
/// from a real `200 OK` registration on Airtel India, not the generic RFC
/// 3329 grammar: one `Security-Client` header whose value is a comma-joined
/// list of `ipsec-3gpp;alg=<alg>;ealg=<ealg>;spi-c=..;spi-s=..;port-c=..;
/// port-s=..` tuples — no spaces around `;`, no `prot=`/`mod=`/`q=`.
///
/// Both `ealg=aes-cbc` and `ealg=null` are offered for each integrity
/// algorithm. The `null` tuples are what the captured working Airtel
/// REGISTER proposed; the `aes-cbc` tuples are REQUIRED by Vodafone India,
/// whose P-CSCF rejects any Security-Client offering only `ealg=null` with
/// an instant blanket `403 Forbidden` — no challenge, no Security-Server,
/// identical bytes regardless of every other header (bisected live with a
/// raw-REGISTER prober; the moment an `aes-cbc` tuple appears, the same
/// request gets the real `401` + AKA challenge and a populated
/// `Security-Server` selecting `alg=hmac-sha-1-96; ealg=aes-cbc`).
/// `des-ede3-cbc` is deliberately NOT offered: its 192-bit key needs the
/// TS 33.203 Annex I CK-expansion we don't implement, and a network could
/// legitimately select it if offered. `gm_ipsec` keys `aes-cbc` with the
/// AKA CK directly (TS 33.203 Annex H), same as it always did for IK.
fn build_security_client_headers(proposal: &SaProposal) -> Vec<String> {
    const ALGS: [&str; 2] = ["hmac-md5-96", "hmac-sha-1-96"];
    const EALGS: [&str; 2] = ["aes-cbc", "null"];

    let tuples: Vec<String> = ALGS
        .iter()
        .flat_map(|alg| EALGS.iter().map(move |ealg| (alg, ealg)))
        .map(|(alg, ealg)| {
            format!(
                "ipsec-3gpp;alg={alg};ealg={ealg};spi-c={};spi-s={};port-c={};port-s={}",
                proposal.spi_c, proposal.spi_s, proposal.port_c, proposal.port_s
            )
        })
        .collect();

    vec![
        "Supported: sec-agree".to_string(),
        format!("Security-Client: {}", tuples.join(", ")),
    ]
}

fn build_authorization(
    impi_uri: &str,
    challenge: &sip_client::DigestChallenge,
    uri: &str,
    res: &[u8],
) -> String {
    let ha1 = digest::ha1(impi_uri, &challenge.realm, res);
    let ha2 = digest::ha2("REGISTER", uri);

    let (response, qop_params) = match &challenge.qop {
        Some(qop) if qop.contains("auth") => {
            let nc = "00000001";
            let cnonce = random_hex(8);
            let resp = digest::response_qop(&ha1, &challenge.nonce, nc, &cnonce, "auth", &ha2);
            (resp, format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\""))
        }
        _ => (
            digest::response_simple(&ha1, &challenge.nonce, &ha2),
            String::new(),
        ),
    };

    let opaque_param = challenge
        .opaque
        .as_ref()
        .map(|o| format!(", opaque=\"{o}\""))
        .unwrap_or_default();

    format!(
        "Digest username=\"{impi_uri}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm=AKAv1-MD5{qop_params}{opaque_param}",
        realm = challenge.realm,
        nonce = challenge.nonce,
    )
}

fn build_resync_authorization(
    impi_uri: &str,
    challenge: &sip_client::DigestChallenge,
    uri: &str,
    auts: &[u8],
) -> String {
    // RFC 3310 §4.4: use an empty password when the AKA run signaled a sync
    // failure; the resulting response value is not meant to authenticate,
    // it just satisfies the Authorization header's required fields.
    let ha1 = digest::ha1(impi_uri, &challenge.realm, b"");
    let ha2 = digest::ha2("REGISTER", uri);
    let response = digest::response_simple(&ha1, &challenge.nonce, &ha2);
    let auts_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, auts);

    format!(
        "Digest username=\"{impi_uri}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\", algorithm=AKAv1-MD5, auts=\"{auts_b64}\"",
        realm = challenge.realm,
        nonce = challenge.nonce,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn renewal_not_due_when_well_before_expiry() {
        let expires_at = SystemTime::now() + Duration::from_secs(3600);
        let now = SystemTime::now();
        assert!(!renewal_due(now, expires_at, Duration::from_secs(300)));
    }

    #[test]
    fn health_is_can_answer_only_when_registered_attached_and_idle() {
        // The status a `volte-status` query reports is derived from exactly the
        // health this produces, so pin the mapping the dispatch loop relies on.
        let base = RegistrationStatus {
            state: RegistrationState::Registered,
            ..Default::default()
        };
        assert!(base.health().can_answer());
        assert_eq!(base.health().blocked_reason(), None);

        // Busy: blocked, but the attachment/registration are still up.
        let busy = RegistrationStatus {
            busy: true,
            ..base.clone()
        };
        assert!(!busy.health().can_answer());
        assert_eq!(
            busy.health().blocked_reason(),
            Some("a call is already in progress")
        );

        // Attachment down outranks everything — a card on this path has no
        // circuit-switched fallback, so it must never claim it can answer.
        let detached = RegistrationStatus {
            attached: false,
            ..base.clone()
        };
        assert!(!detached.health().can_answer());
        assert_eq!(
            detached.health().blocked_reason(),
            Some("the network attachment is down")
        );

        // Mid-renewal is not registered enough to answer.
        let renewing = RegistrationStatus {
            state: RegistrationState::Renewing,
            ..base
        };
        assert!(!renewing.health().can_answer());
    }

    #[test]
    fn an_elapsed_registration_cannot_answer_however_healthy_it_looks() {
        // The 2026-08-16 state, exactly: state still says Registered, the Gm
        // connection is up, the attachment is fine — and the binding lapsed
        // hours ago. `can_answer` said yes for 2h45m.
        let now = SystemTime::now();
        let status = RegistrationStatus {
            state: RegistrationState::Registered,
            registered_at: Some(now - Duration::from_secs(3600 + 9900)),
            expires_at: Some(now - Duration::from_secs(9900)),
            ..Default::default()
        };
        let health = status.health_at(now);
        assert!(!health.can_answer());
        assert_eq!(
            health.blocked_reason(),
            Some("the registration has expired"),
            "the reason must name expiry, not the generic 'not registered' that made \
             this take live forensics to diagnose"
        );
    }

    #[test]
    fn expiry_outranks_a_down_signalling_connection_in_the_reason() {
        // Ordering matters: expiry is the cause, a dead Gm connection is a
        // symptom, and reporting the symptom sends an operator to the wrong
        // place.
        let now = SystemTime::now();
        let status = RegistrationStatus {
            state: RegistrationState::Registered,
            expires_at: Some(now - Duration::from_secs(60)),
            gm_connection: GmConnectionState::Failed { since: now },
            ..Default::default()
        };
        assert_eq!(
            status.health_at(now).blocked_reason(),
            Some("the registration has expired")
        );
    }

    #[test]
    fn a_registration_still_inside_its_lifetime_is_unaffected() {
        let now = SystemTime::now();
        let status = RegistrationStatus {
            state: RegistrationState::Registered,
            registered_at: Some(now),
            expires_at: Some(now + Duration::from_secs(3600)),
            ..Default::default()
        };
        let health = status.health_at(now);
        assert!(health.can_answer());
        assert_eq!(health.blocked_reason(), None);
    }

    #[test]
    fn a_registration_with_no_recorded_expiry_is_never_treated_as_expired() {
        // `expires_at: None` means "not known", not "long past" — reading it as
        // expiry would take a healthy line out of service.
        let now = SystemTime::now();
        let status = RegistrationStatus {
            state: RegistrationState::Registered,
            expires_at: None,
            ..Default::default()
        };
        assert!(status.health_at(now).can_answer());
    }

    #[test]
    fn a_short_granted_lifetime_scales_the_headroom_instead_of_renewing_constantly() {
        // The trap that opens the moment a granted lifetime is honoured: with a
        // fixed 300s headroom, anything granted under 600s is *always* "due",
        // so the agent re-registers on every idle poll — once a second —
        // forever. Scaling the margin is what makes honouring the grant safe.
        let preferred = Duration::from_secs(300);
        let granted = Duration::from_secs(120);
        let headroom = renewal_headroom_for(granted, preferred);
        assert_eq!(headroom, Duration::from_secs(60));

        let now = SystemTime::now();
        let expires_at = now + granted;
        assert!(
            !renewal_due(now, expires_at, headroom),
            "a freshly granted 120s registration must not already be due"
        );
        assert!(
            renewal_due(now + Duration::from_secs(61), expires_at, headroom),
            "it must still renew comfortably before it lapses"
        );
    }

    #[test]
    fn a_generous_granted_lifetime_keeps_the_preferred_headroom() {
        let preferred = Duration::from_secs(300);
        assert_eq!(
            renewal_headroom_for(Duration::from_secs(3600), preferred),
            preferred
        );
        // Exactly twice the headroom is the boundary where scaling starts.
        assert_eq!(
            renewal_headroom_for(Duration::from_secs(600), preferred),
            preferred
        );
    }

    #[test]
    fn a_600s_grant_renews_at_300s_not_at_the_default_lifetime() {
        let now = SystemTime::now();
        let granted = Duration::from_secs(600);
        let headroom = renewal_headroom_for(granted, Duration::from_secs(300));
        let expires_at = now + granted;
        assert!(!renewal_due(now, expires_at, headroom));
        assert!(
            renewal_due(now + Duration::from_secs(301), expires_at, headroom),
            "must renew 300s in, not wait as if the lifetime were an hour"
        );
    }

    #[test]
    fn a_response_without_an_expires_header_falls_back_to_what_was_requested() {
        // The current carriers all grant the hour asked for, so this is the
        // path in production today: behaviour must be unchanged.
        let headers = vec![("Via".to_string(), "SIP/2.0/TCP 10.0.0.1".to_string())];
        assert_eq!(granted_expires(&headers, DEFAULT_EXPIRES), DEFAULT_EXPIRES);
        assert_eq!(
            renewal_headroom_for(
                Duration::from_secs(DEFAULT_EXPIRES as u64),
                Duration::from_secs(300)
            ),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn renewal_due_once_inside_the_headroom_window() {
        let now = SystemTime::now();
        let expires_at = now + Duration::from_secs(200);
        assert!(renewal_due(now, expires_at, Duration::from_secs(300)));
    }

    #[test]
    fn renewal_due_exactly_at_the_headroom_boundary() {
        let now = SystemTime::now();
        let expires_at = now + Duration::from_secs(300);
        assert!(renewal_due(now, expires_at, Duration::from_secs(300)));
    }

    #[test]
    fn renewal_due_when_already_past_expiry() {
        let now = SystemTime::now();
        let expires_at = now - Duration::from_secs(10);
        assert!(renewal_due(now, expires_at, Duration::from_secs(300)));
    }

    #[test]
    fn renewal_due_when_headroom_exceeds_time_to_expiry() {
        // expires_at is so close that subtracting headroom would underflow
        // (or would if SystemTime allowed negative time) — must still
        // report "due", not panic or silently say "not due".
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let expires_at = SystemTime::UNIX_EPOCH + Duration::from_secs(150);
        assert!(renewal_due(now, expires_at, Duration::from_secs(300)));
    }

    #[test]
    fn registration_status_defaults_to_unregistered() {
        let status = RegistrationStatus::default();
        assert_eq!(status.state, RegistrationState::Unregistered);
        assert!(status.registered_at.is_none());
        assert!(status.expires_at.is_none());
        assert!(status.last_failure.is_none());
    }

    #[test]
    fn register_request_uri_uses_the_pcscf_address_when_home_domain_is_off() {
        // No longer the default (see `VowifiConfig::register_request_uri`), but
        // kept reachable as the escape hatch for Airtel, which is recorded —
        // unreproduced — as answering `406 User Unknown` to the realm form.
        let pcscf: SocketAddr = "56.2.134.134:5060".parse().unwrap();
        let uri = register_request_uri(pcscf, "ims.mnc869.mcc405.3gppnetwork.org", false);
        assert_eq!(uri, "56.2.134.134:5060");
    }

    #[test]
    fn register_request_uri_uses_the_realm_when_home_domain_is_set() {
        // Jio's P-CSCF answers 483/403 to the address form — see this
        // function's docs for the live bisection.
        let pcscf: SocketAddr = "56.2.134.134:5060".parse().unwrap();
        let uri = register_request_uri(pcscf, "ims.mnc869.mcc405.3gppnetwork.org", true);
        assert_eq!(uri, "ims.mnc869.mcc405.3gppnetwork.org");
        assert!(
            !uri.contains("56.2.134.134"),
            "the P-CSCF address must not leak into the request line"
        );
    }

    #[test]
    fn register_request_uri_wraps_ipv6_pcscf_addresses() {
        let pcscf: SocketAddr = "[2405:200:6ae::1e]:5060".parse().unwrap();
        let uri = register_request_uri(pcscf, "ims.mnc869.mcc405.3gppnetwork.org", false);
        assert_eq!(uri, "[2405:200:6ae::1e]:5060");
    }

    #[test]
    fn security_client_header_includes_proposal_values() {
        let proposal = SaProposal {
            spi_c: 111,
            spi_s: 222,
            port_c: 5062,
            port_s: 5064,
        };
        let headers = build_security_client_headers(&proposal);
        assert_eq!(headers[0], "Supported: sec-agree");
        let sc = &headers[1];
        assert!(sc.starts_with("Security-Client: ipsec-3gpp"));
        assert!(sc.contains("alg=hmac-md5-96"));
        assert!(sc.contains("alg=hmac-sha-1-96"));
        assert!(sc.contains("ealg=null"));
        // Vodafone India 403s any offer without an aes-cbc tuple — see
        // build_security_client_headers' docs.
        assert!(sc.contains("ealg=aes-cbc"));
        assert!(!sc.contains("des-ede3"));
        assert!(sc.contains("spi-c=111"));
        assert!(sc.contains("spi-s=222"));
        assert!(sc.contains("port-c=5062"));
        assert!(sc.contains("port-s=5064"));
        assert!(!sc.contains(" ;"));
        assert!(!sc.contains("prot="));
        assert!(!sc.contains("mod="));
        assert!(!sc.contains("q="));
    }

    #[test]
    fn build_authorization_uses_qop_when_offered() {
        let challenge = sip_client::DigestChallenge {
            realm: "ims.mnc043.mcc404.3gppnetwork.org".to_string(),
            nonce: "bm9uY2U=".to_string(),
            qop: Some("auth".to_string()),
            opaque: None,
            algorithm: Some("AKAv1-MD5".to_string()),
        };
        let auth = build_authorization(
            "404438083996440@ims.mnc043.mcc404.3gppnetwork.org",
            &challenge,
            "sip:ims.mnc043.mcc404.3gppnetwork.org",
            b"\x01\x02\x03\x04\x05\x06\x07\x08",
        );
        assert!(auth.contains("qop=auth"));
        assert!(auth.contains("nc=00000001"));
        assert!(auth.contains("cnonce="));
        assert!(auth.contains("algorithm=AKAv1-MD5"));
    }

    #[test]
    fn build_authorization_omits_qop_when_not_offered() {
        let challenge = sip_client::DigestChallenge {
            realm: "realm".to_string(),
            nonce: "bm9uY2U=".to_string(),
            qop: None,
            opaque: Some("op41234".to_string()),
            algorithm: Some("AKAv1-MD5".to_string()),
        };
        let auth = build_authorization("user@realm", &challenge, "sip:realm", b"12345678");
        assert!(!auth.contains("qop="));
        assert!(auth.contains("opaque=\"op41234\""));
    }

    #[test]
    fn build_resync_authorization_includes_auts_and_empty_password_digest() {
        let challenge = sip_client::DigestChallenge {
            realm: "realm".to_string(),
            nonce: "bm9uY2U=".to_string(),
            qop: None,
            opaque: None,
            algorithm: Some("AKAv1-MD5".to_string()),
        };
        let auts = [0xABu8; 14];
        let auth = build_resync_authorization("user@realm", &challenge, "sip:realm", &auts);
        let expected_auts_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, auts);
        assert!(auth.contains(&format!("auts=\"{expected_auts_b64}\"")));
        // response computed with an empty password, not the (absent) RES
        let expected_ha1 = digest::ha1("user@realm", "realm", b"");
        let expected_ha2 = digest::ha2("REGISTER", "sip:realm");
        let expected_response = digest::response_simple(&expected_ha1, "bm9uY2U=", &expected_ha2);
        assert!(auth.contains(&format!("response=\"{expected_response}\"")));
    }

    /// Regression test for a live crash-loop: `vowifi-usim-bridge` holds the
    /// modem's serial for as long as charon keeps the virtual card powered on,
    /// so Agent A losing that race is normal and transient. It used to fail the
    /// agent outright, tearing down and rebuilding the whole IMS session over a
    /// conflict that clears in seconds.
    #[test]
    fn a_busy_modem_serial_is_waited_out_rather_than_failing_the_session() {
        let mut attempts = 0;
        let mut slept = Vec::new();
        let opened = open_modem_waiting(
            "/dev/ttyUSB0",
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(2),
            || {
                attempts += 1;
                if attempts < 3 {
                    Err("Unable to acquire exclusive lock on serial port")
                } else {
                    Ok("commander")
                }
            },
            |d| slept.push(d),
        );
        assert_eq!(opened, Ok("commander"));
        assert_eq!(attempts, 3);
        assert_eq!(slept.len(), 2);
    }

    /// Bounded on purpose: a genuinely wrong or missing port must still surface
    /// as an error instead of hanging the agent forever.
    #[test]
    fn a_permanently_unavailable_modem_serial_still_gives_up() {
        let mut attempts = 0;
        let mut slept = Vec::new();
        let opened: Result<&str, &str> = open_modem_waiting(
            "/dev/ttyUSB9",
            std::time::Duration::from_secs(6),
            std::time::Duration::from_secs(2),
            || {
                attempts += 1;
                Err("No such file or directory")
            },
            |d| slept.push(d),
        );
        assert_eq!(opened, Err("No such file or directory"));
        assert_eq!(slept.len(), 3, "waits the full budget, then reports");
    }

    #[test]
    fn an_available_modem_serial_is_opened_without_delay() {
        let mut slept = Vec::new();
        let opened = open_modem_waiting(
            "/dev/ttyUSB0",
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(2),
            || Ok::<_, String>("commander"),
            |d| slept.push(d),
        );
        assert_eq!(opened, Ok("commander"));
        assert!(slept.is_empty(), "a free port must not delay registration");
    }
}
