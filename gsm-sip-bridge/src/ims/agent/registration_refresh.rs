//! A REGISTER refresh on the live Gm connection (TS 24.229 §5.1.1.4).
//!
//! The transaction belongs to the dispatch loop: it is sent, retransmitted and
//! timed out from the loop's passes, and its answer arrives through the loop's
//! single reader. Nothing here blocks, so calls and notifications keep being
//! served while a refresh is outstanding.
//!
//! The Contact-lifetime parsing below is adapted from the reporter's PR #168
//! (commit 3e914c3, Kirill Mironov).

use super::{map_registration_error, LoopState};
use crate::control::protocol::RegistrationStatus;
use crate::error::{BridgeError, BridgeResult};
use crate::ims::gm_ipsec::{
    self, GmEndpoints, IpXfrm, RetiredSas, SaKeys, SaSet, StagedSas, XfrmBackend,
};
use crate::ims::observability::AgentObservability;
use crate::ims::registration_auth::{Answer, ChallengeJob, Reauthentication};
use crate::ims::session::ClientReader;
use crate::ims::session::Inbound;
use crate::ims::session::RegisterFailure;
use crate::ims::sip_client::SipTransport;
use crate::ims::sip_client::{
    build_register, random_hex, RegisterRequest, SipMessage, SipResponse, SipSink,
};
use crate::ims::GmSa;
use crate::ims::{
    build_security_client_headers, register_request_uri, ImsRegisterConfig, RegisteredSession,
    RegistrationAuth, SaProposal, DEFAULT_EXPIRES,
};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// TS 24.229 table 7.7.1 overrides the base SIP timers for IMS access: Timer F
/// (the whole transaction) is 128s, T1 2s, T2 16s.
pub(super) const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(128);
const T1: Duration = Duration::from_secs(2);
const T2: Duration = Duration::from_secs(16);

/// One outstanding refresh REGISTER.
pub(super) struct PendingRegister {
    pub(super) call_id: String,
    pub(super) cseq: u32,
    branch: String,
    request: String,
    pub(super) requested_expires: u32,
    deadline: Instant,
    retransmit_at: Instant,
    retransmit_interval: Duration,
    use_tcp: bool,
    provisional: bool,
    interval_retry: bool,
    /// The security association proposed with this refresh (RFC 3329): the
    /// protected server port is the one in use, the client port and both SPIs
    /// are new (TS 33.203 §7.4). `None` when the registration has no Gm SA.
    proposal: Option<SaProposal>,
    /// Holds the proposed client port while the transaction is outstanding, so
    /// nothing else binds it. Never read; dropped with the transaction.
    _reserved_port: Option<Socket>,
    /// Which step of the procedure this REGISTER is.
    kind: Kind,
    /// The `Authorization` to send instead of the cached one: the answer to a
    /// challenge, or the resynchronisation request.
    authorization: Option<String>,
    /// How many times the network has been asked to resynchronise.
    resyncs: u32,
    /// The replacement connection and security associations this REGISTER is
    /// sent over, once a challenge has produced new keys.
    swap: Option<Swap>,
}

/// The steps of a refresh the network turns into a re-authentication
/// (TS 24.229 §5.1.1.4.2, §5.1.1.5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// The ordinary refresh, with the cached authorization.
    Refresh,
    /// Sent with AUTS because the SIM's sequence number is behind the
    /// network's; the network answers with a fresh challenge.
    Resync,
    /// Carries the SIM's answer to a challenge.
    Answer,
}

/// A replacement for the client connection and the security associations under
/// it, built from a challenge and not yet carrying anything. Dropped before the
/// network confirms it, everything is undone and the working connection was
/// never touched.
pub(super) struct Swap {
    staged: Option<StagedSas>,
    transport: Option<SipTransport>,
    reader: Option<ClientReader>,
    sa: Option<GmSa>,
}

impl Swap {
    /// The parts, once the network has confirmed the replacement.
    fn into_parts(mut self) -> (StagedSas, SipTransport, ClientReader, GmSa) {
        (
            self.staged.take().expect("parts are taken once"),
            self.transport.take().expect("parts are taken once"),
            self.reader.take().expect("parts are taken once"),
            self.sa.take().expect("parts are taken once"),
        )
    }
}

impl Drop for Swap {
    fn drop(&mut self) {
        // An abandoned replacement: the connection is closed for every handle
        // (the reader holds one), the reader told to go, and `staged`, dropped
        // after this, removes the associations it installed.
        if let Some(transport) = self.transport.take() {
            transport.abort();
        }
        if let Some(reader) = self.reader.take() {
            reader.stop();
            reader.reap();
        }
    }
}

/// What a REGISTER in the procedure carries over from the one before it.
struct Carry {
    proposal: Option<SaProposal>,
    reserved: Option<Socket>,
    resyncs: u32,
    requested_expires: u32,
}

/// Binds a socket on an unused port beside `local` and keeps it, so the port
/// can be proposed as the new protected client port.
fn reserve_client_port(local: SocketAddr, use_tcp: bool) -> BridgeResult<(Socket, u16)> {
    let socket = Socket::new(
        Domain::for_address(local),
        if use_tcp { Type::STREAM } else { Type::DGRAM },
        Some(if use_tcp {
            Protocol::TCP
        } else {
            Protocol::UDP
        }),
    )
    .map_err(|e| BridgeError::Ims(format!("reserve a client port for the refresh: {e}")))?;
    socket
        .bind(&SocketAddr::new(local.ip(), 0).into())
        .map_err(|e| BridgeError::Ims(format!("bind a client port for the refresh: {e}")))?;
    let port = socket
        .local_addr()
        .ok()
        .and_then(|address| address.as_socket())
        .ok_or_else(|| BridgeError::Ims("the reserved client port has no address".into()))?
        .port();
    Ok((socket, port))
}

impl PendingRegister {
    /// Sends the refresh on the session's live connection with the cached
    /// authorization (TS 24.229 §5.1.1.4.2 a): the last received nonce and the
    /// last calculated response. Errors when the registration carries no cached
    /// authorization, which the caller treats as "needs a full registration".
    pub(super) fn start(
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
    ) -> BridgeResult<Self> {
        if session.registration_auth.is_none() {
            return Err(BridgeError::Ims(
                "no cached authorization to refresh the registration with".into(),
            ));
        }
        let mut carry = Carry {
            proposal: None,
            reserved: None,
            resyncs: 0,
            requested_expires: DEFAULT_EXPIRES,
        };
        if let Some(gm) = &session.gm_state {
            let (socket, port_c) = reserve_client_port(session.local_addr, session.use_tcp)?;
            carry.proposal = Some(SaProposal {
                spi_c: rand::random::<u32>() | 1,
                spi_s: rand::random::<u32>() | 1,
                port_c,
                port_s: gm.proposal.port_s,
            });
            carry.reserved = Some(socket);
        }
        Self::launch(session, cfg, now, carry, Kind::Refresh, None, None)
    }

    /// Sends the next REGISTER of the procedure: the refresh itself, the
    /// resynchronisation request, or the answer to a challenge.
    fn launch(
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
        carry: Carry,
        kind: Kind,
        authorization: Option<String>,
        swap: Option<Swap>,
    ) -> BridgeResult<Self> {
        let mut pending = Self {
            call_id: session.call_id.clone(),
            cseq: 0,
            branch: String::new(),
            request: String::new(),
            requested_expires: carry.requested_expires,
            deadline: now + TRANSACTION_TIMEOUT,
            retransmit_at: now + T1,
            retransmit_interval: T1,
            use_tcp: session.use_tcp,
            provisional: false,
            interval_retry: false,
            proposal: carry.proposal,
            _reserved_port: carry.reserved,
            kind,
            authorization,
            resyncs: carry.resyncs,
            swap,
        };
        pending.send_new(session, cfg, now)?;
        Ok(pending)
    }

    /// The connection this REGISTER goes out on: the replacement, once there is
    /// one, otherwise the session's.
    fn out<'a>(
        &'a mut self,
        session: &'a mut RegisteredSession,
    ) -> BridgeResult<&'a mut SipTransport> {
        match self.swap.as_mut().and_then(|swap| swap.transport.as_mut()) {
            Some(transport) => Ok(transport),
            None => session.transport_mut(),
        }
    }

    /// Builds and sends a new transaction (new branch, next REGISTER CSeq).
    fn send_new(
        &mut self,
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
    ) -> BridgeResult<()> {
        let authorization = match &self.authorization {
            Some(authorization) => authorization.clone(),
            None => session
                .registration_auth
                .as_ref()
                .map(|auth| auth.authorization.clone())
                .ok_or_else(|| BridgeError::Ims("cached authorization is gone".into()))?,
        };
        // Where the answer to a challenge is sent from: the replacement
        // connection's own address, not the working one's.
        let local_addr = match self.swap.as_ref().and_then(|swap| swap.transport.as_ref()) {
            Some(transport) => transport.local_addr()?,
            None => session.local_addr,
        };
        let security_verify = match self.swap.as_ref().and_then(|swap| swap.sa.as_ref()) {
            Some(sa) => Some(sa.security_verify.clone()),
            None => session
                .gm_state
                .as_ref()
                .map(|gm| gm.security_verify.clone()),
        };
        self.cseq = session.registration_cseq;
        session.registration_cseq = session
            .registration_cseq
            .checked_add(1)
            .ok_or_else(|| BridgeError::Ims("REGISTER CSeq exhausted".into()))?;
        self.branch = format!("z9hG4bK{}", random_hex(6));
        let mut extra = vec![format!(
            "P-Access-Network-Info: {}",
            cfg.access_network_info
        )];
        if let (Some(verify), Some(proposal)) = (&security_verify, &self.proposal) {
            // The security-agreement headers a UE sends on any re-registration
            // (RFC 3329, TS 24.229 §5.1.1.4.2): the protected server port stays,
            // the client port and SPIs are new. The network only acts on them if
            // it challenges, which is answered in the background.
            extra.extend([
                "Require: sec-agree".to_string(),
                "Proxy-Require: sec-agree".to_string(),
            ]);
            extra.extend(build_security_client_headers(proposal));
            extra.push(format!("Security-Verify: {verify}"));
        }
        self.request = build_register(&RegisterRequest {
            registrar_uri: &register_request_uri(
                session.pcscf_addr,
                &session.realm,
                cfg.register_uri_home_domain,
            ),
            public_uri: &session.public_uri,
            local_addr,
            contact_addr: session.contact_addr,
            call_id: &self.call_id,
            from_tag: &session.from_tag,
            branch: &self.branch,
            cseq: self.cseq,
            expires: self.requested_expires,
            transport: if session.use_tcp { "TCP" } else { "UDP" },
            authorization: Some(&authorization),
            extra_headers: &extra,
            imei: &session.imei,
        });
        let request = self.request.clone();
        self.out(session)?.send(&request)?;
        self.deadline = now + TRANSACTION_TIMEOUT;
        self.retransmit_at = now + T1;
        self.retransmit_interval = T1;
        self.provisional = false;
        Ok(())
    }

    /// Moves Timer F to `deadline`, so a test can stand in for the time that
    /// passed while the loop was busy.
    #[cfg(test)]
    pub(super) fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    /// Whether `response` answers this very transaction: same Call-ID, same
    /// CSeq number and method, and exactly one `branch` in the top Via. Matching
    /// on the CSeq number alone would let an OPTIONS probe's answer (which
    /// shares the registration Call-ID) or a retired attempt's answer confirm it.
    pub(super) fn matches(&self, response: &SipResponse) -> bool {
        response.header("Call-ID") == Some(self.call_id.as_str())
            && response.header("CSeq").is_some_and(|value| {
                let mut parts = value.split_whitespace();
                parts.next().and_then(|n| n.parse::<u32>().ok()) == Some(self.cseq)
                    && parts
                        .next()
                        .is_some_and(|method| method.eq_ignore_ascii_case("REGISTER"))
                    && parts.next().is_none()
            })
            && response.header("Via").is_some_and(|via| {
                let mut branches = via
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .split(';')
                    .skip(1)
                    .filter_map(|parameter| {
                        let (name, value) = parameter.split_once('=')?;
                        name.trim()
                            .eq_ignore_ascii_case("branch")
                            .then_some(value.trim())
                    });
                branches.next() == Some(self.branch.as_str()) && branches.next().is_none()
            })
    }

    /// A provisional answer: retransmission slows to T2, but Timer F is not
    /// extended.
    pub(super) fn provisional(&mut self) {
        self.provisional = true;
    }

    /// Drives retransmission and Timer F. `Ok(false)` means Timer F expired.
    /// Only UDP retransmits (RFC 3261 §17.1.2.2); Timer F applies to both.
    pub(super) fn poll(
        &mut self,
        session: &mut RegisteredSession,
        now: Instant,
    ) -> BridgeResult<bool> {
        if now >= self.deadline {
            return Ok(false);
        }
        if !self.use_tcp && now >= self.retransmit_at {
            let request = self.request.clone();
            self.out(session)?.send(&request)?;
            self.retransmit_interval = if self.provisional {
                T2
            } else {
                (self.retransmit_interval * 2).min(T2)
            };
            self.retransmit_at = now + self.retransmit_interval;
        }
        Ok(true)
    }

    /// `423 Interval Too Brief`: one retry with at least `Min-Expires`
    /// (TS 24.229 §5.1.1.4.1). `Ok(false)` when there is nothing to retry.
    pub(super) fn retry_min_expires(
        &mut self,
        response: &SipResponse,
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
    ) -> BridgeResult<bool> {
        let Some(minimum) = response
            .header("Min-Expires")
            .and_then(|value| delta_seconds(value).ok())
        else {
            return Ok(false);
        };
        if self.interval_retry || minimum <= self.requested_expires {
            return Ok(false);
        }
        self.interval_retry = true;
        self.requested_expires = minimum;
        self.send_new(session, cfg, now)?;
        Ok(true)
    }
}

fn delta_seconds(value: &str) -> BridgeResult<u32> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BridgeError::Ims("malformed registration expiry".into()));
    }
    value
        .parse()
        .map_err(|_| BridgeError::Ims("registration expiry exceeds u32".into()))
}

/// Header parameters follow the closing `>` for a name-addr, while bare
/// Contact addresses end their URI before the first parameter. The URI slice
/// returned by parse_name_addr borrows the original value, so its end gives
/// the exact boundary even with `>` inside a quoted display name or instance.
fn contact_expiry(value: &str, uri: &str) -> BridgeResult<Option<u32>> {
    let uri_end = uri.as_ptr() as usize - value.as_ptr() as usize + uri.len();
    let suffix = value[uri_end..].trim_start();
    let parameters = suffix.strip_prefix('>').unwrap_or(suffix);
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    let mut expires = None;
    let mut read_parameter = |parameter: &str| -> BridgeResult<()> {
        let parameter = parameter.trim();
        let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        if key.trim().eq_ignore_ascii_case("expires") {
            if expires.is_some() {
                return Err(BridgeError::Ims(
                    "duplicate Contact expires parameter".into(),
                ));
            }
            expires = Some(delta_seconds(value)?);
        }
        Ok(())
    };
    for (index, ch) in parameters.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
        } else if ch == ';' {
            read_parameter(&parameters[start..index])?;
            start = index + 1;
        }
    }
    if quoted {
        return Err(BridgeError::Ims("unterminated Contact parameter".into()));
    }
    read_parameter(&parameters[start..])?;
    Ok(expires)
}

fn contact_endpoint(uri: &str) -> Option<SocketAddr> {
    let (scheme, address) = uri.split_once(':')?;
    if !scheme.eq_ignore_ascii_case("sip") {
        return None;
    }
    let address = address.rsplit_once('@').map_or(address, |(_, host)| host);
    let address = address.split([';', '?']).next()?;
    address.parse().ok().or_else(|| {
        let ip = if let Some(ipv6) = address
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
        {
            std::net::IpAddr::V6(ipv6.parse().ok()?)
        } else {
            std::net::IpAddr::V4(address.parse().ok()?)
        };
        Some(SocketAddr::new(ip, 5060))
    })
}

/// A refresh response can list other devices first. Only our Contact's header
/// expires parameter changes this binding; the response Expires is a fallback.
/// A Contact list omitting our endpoint cannot confirm a successful renewal.
pub(super) fn granted_lifetime(
    response: &SipResponse,
    contact: SocketAddr,
    requested: u32,
) -> BridgeResult<u32> {
    use crate::ims::identity::{parse_name_addr, split_header_values, HeaderParams};
    let contacts = response.headers_all("Contact");
    let mut own = None;
    for value in contacts.iter().flat_map(|value| split_header_values(value)) {
        let Some(parsed) = parse_name_addr(value, HeaderParams::Allowed) else {
            continue;
        };
        if contact_endpoint(parsed.uri) != Some(contact) {
            continue;
        }
        let lifetime = contact_expiry(value, parsed.uri)?;
        if own.replace(lifetime).is_some() {
            return Err(BridgeError::Ims(
                "duplicate registration Contact binding".into(),
            ));
        }
    }
    if !contacts.is_empty() && own.is_none() {
        return Err(BridgeError::Ims(
            "REGISTER response omitted our Contact binding".into(),
        ));
    }
    let expires = match own.flatten() {
        Some(expires) => expires,
        None => {
            let values = response.headers_all("Expires");
            match values.as_slice() {
                [] => requested,
                [value] => delta_seconds(value)?,
                _ => {
                    return Err(BridgeError::Ims(
                        "duplicate registration Expires header".into(),
                    ))
                }
            }
        }
    };
    Ok(expires)
}

// --- the dispatch loop's side of a refresh -----------------------------------

/// The Gm connection as the refresh sees it: the queue its answers arrive on,
/// and the way to rebuild the client connection when the network has closed it.
pub(super) trait GmLink {
    fn queue(&self) -> &std::sync::mpsc::Receiver<(SipMessage, SipSink)>;
    /// Whether the client connection is still open from our side: its reader
    /// ends when the network closes it.
    fn client_alive(&self) -> bool;
    /// Rebuilds the client transport on the still-live security association.
    fn repair(&mut self, session: &mut RegisteredSession) -> BridgeResult<()>;
    /// Starts a reader on a client connection that is not yet the session's.
    fn read_connection(&self, transport: &SipTransport) -> BridgeResult<ClientReader>;
    /// Makes `reader` the live client reader, returning the one it replaces.
    fn adopt_reader(&self, reader: ClientReader) -> ClientReader;
}

impl GmLink for Inbound {
    fn queue(&self) -> &std::sync::mpsc::Receiver<(SipMessage, SipSink)> {
        &self.rx
    }

    fn client_alive(&self) -> bool {
        self.client_reader_alive()
    }

    fn repair(&mut self, session: &mut RegisteredSession) -> BridgeResult<()> {
        super::ping::reconnect_gm_client(session, self)
    }
    fn read_connection(&self, transport: &SipTransport) -> BridgeResult<ClientReader> {
        crate::ims::session::read_client_connection(transport, self)
    }
    fn adopt_reader(&self, reader: ClientReader) -> ClientReader {
        crate::ims::session::adopt_client_reader(self, reader)
    }
}

impl GmLink for &Inbound {
    fn queue(&self) -> &std::sync::mpsc::Receiver<(SipMessage, SipSink)> {
        (**self).queue()
    }

    fn client_alive(&self) -> bool {
        (**self).client_alive()
    }

    fn repair(&mut self, session: &mut RegisteredSession) -> BridgeResult<()> {
        super::ping::reconnect_gm_client(session, self)
    }

    fn read_connection(&self, transport: &SipTransport) -> BridgeResult<ClientReader> {
        (**self).read_connection(transport)
    }

    fn adopt_reader(&self, reader: ClientReader) -> ClientReader {
        (**self).adopt_reader(reader)
    }
}

/// Keeps the refresh running while an inbound call rings and the dispatch
/// loop is parked: answers and timers are serviced from inside the ring wait,
/// by the same code the loop runs. The line counts as busy for as long as this
/// exists, so a failure that would end the agent is deferred until the line is
/// idle, as it is during a connected call.
pub(super) struct RingRefresh<'a> {
    state: &'a mut LoopState,
    ctx: RefreshCtx<'a>,
    link: &'a Inbound,
}

impl<'a> RingRefresh<'a> {
    pub(super) fn new(state: &'a mut LoopState, ctx: RefreshCtx<'a>, link: &'a Inbound) -> Self {
        state.ringing = true;
        Self { state, ctx, link }
    }

    /// An error the refresh cannot recover from. Never ends the call: the exit
    /// is owed until the line is idle.
    fn defer(&mut self, e: BridgeError) {
        tracing::warn!(error = %e, "registration refresh failed while a call rang; exiting once the line is idle");
        self.state.budget_exit_owed.get_or_insert(e);
    }
}

impl Drop for RingRefresh<'_> {
    fn drop(&mut self) {
        self.state.ringing = false;
    }
}

impl super::inbound::RingService for RingRefresh<'_> {
    fn answer(&mut self, session: &mut RegisteredSession, response: &SipResponse) -> bool {
        let mut link = self.link;
        match self
            .state
            .handle_registration_answer(session, &self.ctx, &mut link, response)
        {
            Ok(consumed) => consumed,
            Err(e) => {
                self.defer(e);
                true
            }
        }
    }

    fn tick(&mut self, session: &mut RegisteredSession) {
        let mut link = self.link;
        if let Err(e) = self
            .state
            .drive_registration_refresh(session, &self.ctx, &mut link)
        {
            self.defer(e);
        }
    }
}

/// What the refresh glue needs of the dispatch loop's parameters: the shared
/// registration status, the reporter, and the registration config. A narrow
/// view, so the logic can be driven without building a whole agent.
pub(super) struct RefreshCtx<'a> {
    pub(super) status: &'a Arc<Mutex<crate::ims::RegistrationStatus>>,
    pub(super) obs: &'a AgentObservability,
    pub(super) reg_cfg: &'a ImsRegisterConfig,
}

/// Consecutive rejections of the in-place refresh after which the next cycle
/// uses the full re-registration instead (FR-018a). A starting default, to be
/// revisited after live testing.
pub(super) const REJECTIONS_BEFORE_FALLBACK: u32 = 2;

/// Statuses that mean an in-place refresh cannot work and the UE must perform
/// the initial registration procedure (TS 24.229 §5.1.1.4.1 for 403, 408, 500
/// and 504). A 401 to the refresh is a fresh authentication challenge and is
/// answered in the background; a 401 to the answer itself ends up here.
fn needs_full_registration(status: Option<u16>) -> bool {
    matches!(status, Some(401 | 403 | 408 | 500 | 504))
}

/// A final error answer to the refresh itself. Timer F expiry and transport
/// failures carry no status and are not rejections; a 503 is the proxy being
/// unavailable, which the pool logic handles.
fn is_rejection(failure: &RegisterFailure) -> bool {
    failure.status.is_some_and(|status| status != 503)
}

impl LoopState {
    /// Whether the scheduled refresh should go out in place now, on the live
    /// connection. It cannot when something already asked for a full
    /// re-registration, after repeated rejections, with no cached credentials,
    /// or once the binding has lapsed and no call is holding the line (a full
    /// registration is then the right way back).
    pub(super) fn in_place_refresh_possible(
        &self,
        session: &RegisteredSession,
        expires_at: Option<SystemTime>,
    ) -> bool {
        if self.force_renewal || self.refresh_fallback || session.registration_auth.is_none() {
            return false;
        }
        let lapsed = expires_at.is_some_and(|expiry| SystemTime::now() >= expiry);
        !lapsed || self.busy()
    }

    /// The start of a dispatch pass: after a handler that parked the loop,
    /// takes everything that queued up meanwhile so it is handled before any
    /// timer is judged, then drives the refresh unless messages are waiting.
    pub(super) fn begin_pass(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
    ) -> BridgeResult<()> {
        if std::mem::take(&mut self.after_blocking_handler) {
            while let Ok(message) = link.queue().try_recv() {
                self.held.push_back(message);
            }
        }
        if self.held.is_empty() {
            self.drive_registration_refresh(session, p, link)?;
        }
        Ok(())
    }

    /// Runs once per dispatch pass: retransmits or times out the refresh in
    /// flight, or starts one that has fallen due.
    pub(super) fn drive_registration_refresh(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
    ) -> BridgeResult<()> {
        let now = Instant::now();
        self.drive_retired(now, self.origination.is_some());
        if self.challenged.is_some() {
            return self.drive_challenge(session, p, link, now);
        }
        if let Some(mut pending) = self.registration_refresh.take() {
            return match pending.poll(session, now) {
                Ok(true) => {
                    self.registration_refresh = Some(pending);
                    Ok(())
                }
                Ok(false) => self.refresh_not_carried(
                    session,
                    p,
                    link,
                    BridgeError::Ims(
                        "IMS registration refresh timed out waiting for a SIP response (Timer F)"
                            .into(),
                    )
                    .into(),
                    pending.kind,
                ),
                Err(e) => self.refresh_not_carried(session, p, link, e.into(), pending.kind),
            };
        }
        let (expires_at, registered_at) = {
            let guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            (guard.expires_at, guard.registered_at)
        };
        let Some(expiry) = expires_at else {
            return Ok(());
        };
        if !crate::ims::registration_refresh_due(SystemTime::now(), registered_at, expiry)
            || !self.in_place_refresh_possible(session, expires_at)
            || self.next_renewal_attempt.is_some_and(|at| now < at)
        {
            return Ok(());
        }
        if !link.client_alive() {
            return self.refresh_not_carried(
                session,
                p,
                link,
                BridgeError::Ims("the Gm client connection was closed by the network".into())
                    .into(),
                Kind::Refresh,
            );
        }
        match PendingRegister::start(session, p.reg_cfg, now) {
            Ok(pending) => {
                tracing::info!(
                    cseq = pending.cseq,
                    in_call = self.busy(),
                    "refreshing the registration in place"
                );
                self.registration_refresh = Some(pending);
                Ok(())
            }
            Err(e) => self.refresh_not_carried(session, p, link, e.into(), Kind::Refresh),
        }
    }

    /// The refresh could not be sent, or went unanswered. Without traffic to
    /// keep it open, the network closes an idle Gm connection (seen on Jio
    /// within an hour), and the full renewal that used to run here hid that by
    /// opening a new one. So the first time, rebuild the client connection on
    /// the live security association and send the refresh again, rather than
    /// blaming the P-CSCF. A second failure is handled as any other.
    fn refresh_not_carried(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
        failure: RegisterFailure,
        step: Kind,
    ) -> BridgeResult<()> {
        self.registration_refresh = None;
        // Only the ordinary refresh can be mended by rebuilding the connection
        // and sending it again. A step of the re-authentication that was not
        // carried is a procedure the network is not following: start over with
        // a full registration.
        if matches!(step, Kind::Resync | Kind::Answer) {
            return self.fall_back_to_full(failure.error.to_string());
        }
        // A silent but open connection may be carrying a call's own signalling
        // (an outgoing call's INVITE, say), and rebuilding it would cut that.
        // So while the line is busy, only a connection that is dead or failed
        // a write is rebuilt; a silent one is left to the pool logic.
        let silent_but_open =
            failure.error.to_string().contains("Timer F") && self.busy() && link.client_alive();
        if failure.status.is_none() && !self.refresh_repaired && !silent_but_open {
            self.refresh_repaired = true;
            tracing::warn!(
                error = %failure.error,
                "the connection did not carry the refresh; rebuilding it and sending it again"
            );
            match link.repair(session) {
                Ok(()) => {
                    // Anything pending on the old connection is gone with it.
                    self.ping.reset();
                    return match PendingRegister::start(session, p.reg_cfg, Instant::now()) {
                        Ok(pending) => {
                            self.registration_refresh = Some(pending);
                            Ok(())
                        }
                        Err(e) => self.fail_registration_refresh(session, p, e.into()),
                    };
                }
                Err(e) => {
                    tracing::warn!(error = %e, "rebuilding the Gm connection failed");
                }
            }
        }
        self.fail_registration_refresh(session, p, failure)
    }

    /// Consumes the answer to the refresh in flight. `Ok(false)` when
    /// `response` belongs to something else, so the caller keeps routing it.
    pub(super) fn handle_registration_answer(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
        response: &SipResponse,
    ) -> BridgeResult<bool> {
        let Some(pending) = self.registration_refresh.as_mut() else {
            return Ok(false);
        };
        if !pending.matches(response) {
            return Ok(false);
        }
        if response.status < 200 {
            pending.provisional();
            return Ok(true);
        }
        let mut pending = self
            .registration_refresh
            .take()
            .expect("matched a pending refresh");
        let failure = match response.status {
            200..=299 => {
                return self.confirm_registration_refresh(session, p, link, pending, response)
            }
            401 if pending.kind != Kind::Answer => {
                return self.begin_challenge(session, p, pending, response);
            }
            423 => match pending.retry_min_expires(response, session, p.reg_cfg, Instant::now()) {
                Ok(true) => {
                    self.registration_refresh = Some(pending);
                    return Ok(true);
                }
                Ok(false) => rejected(response),
                Err(e) => e.into(),
            },
            _ => rejected(response),
        };
        self.fail_registration_refresh(session, p, failure)?;
        Ok(true)
    }

    fn confirm_registration_refresh(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
        mut pending: PendingRegister,
        response: &SipResponse,
    ) -> BridgeResult<bool> {
        let granted =
            match granted_lifetime(response, session.contact_addr, pending.requested_expires) {
                Ok(0) => {
                    // Our binding is gone even though the request was
                    // answered: the earlier grant no longer holds, so say so
                    // before the failure is recorded.
                    {
                        let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
                        guard.state = crate::ims::RegistrationState::Failed;
                        guard.expires_at = Some(SystemTime::now());
                    }
                    self.fail_registration_refresh(
                        session,
                        p,
                        BridgeError::Ims("the registrar granted a zero lifetime".into()).into(),
                    )?;
                    self.force_renewal = true;
                    return Ok(true);
                }
                Ok(granted) => granted,
                Err(e) => {
                    self.fail_registration_refresh(session, p, e.into())?;
                    return Ok(true);
                }
            };
        session.status = response.status;
        session.reason = response.reason.clone();
        // Routing for future dialogs follows the latest answer; keep what an
        // answer leaves out rather than losing it.
        let mut headers = response.headers.clone();
        let answered = |name: &str| {
            response
                .headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(name))
        };
        for (name, value) in &session.headers {
            let carried = ["Service-Route", "P-Associated-URI", "Path"]
                .iter()
                .any(|kept| name.eq_ignore_ascii_case(kept));
            if carried && !answered(name) {
                headers.push((name.clone(), value.clone()));
            }
        }
        session.headers = headers;

        let now = SystemTime::now();
        let expiry = now + Duration::from_secs(granted as u64);
        {
            let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            guard.state = crate::ims::RegistrationState::Registered;
            guard.registered_at = Some(now);
            guard.expires_at = Some(expiry);
            guard.attached = true;
        }
        if pending.kind == Kind::Answer {
            if let Some(authorization) = pending.authorization.take() {
                session.registration_auth = Some(RegistrationAuth { authorization });
            }
            if let Some(swap) = pending.swap.take() {
                if let Err(e) = self.activate_swap(session, link, swap) {
                    // The network has accepted the replacement but this side
                    // could not switch to it, so the two disagree about which
                    // associations are live. A full registration settles it.
                    tracing::warn!(error = %e, "could not switch to the new Gm association; registering in full");
                    self.force_renewal = true;
                }
            }
            tracing::info!("registration re-authenticated in place");
        }
        self.on_renewal_success();
        self.last_refresh = Some((now, "ok".to_string()));
        crate::ims::warn_if_short_grant(granted);
        tracing::info!(
            granted_expires_secs = granted,
            "registration refreshed in place"
        );
        p.obs
            .report_registration_attempt(RegistrationStatus::Success);
        p.obs.set_registered(true);
        p.obs.set_registration_expiry(expiry);
        Ok(true)
    }

    /// A refresh did not succeed. The binding the network granted earlier is
    /// still good until it expires, so the line keeps serving calls: nothing
    /// here tears the session down. What happens next follows TS 24.229
    /// §5.1.1.4.1 — a full re-registration for 403/408/500/504 (and 401), the
    /// pool's failover or backoff for everything else — and two rejections in
    /// a row send the following cycle down the full path too.
    pub(super) fn fail_registration_refresh(
        &mut self,
        session: &RegisteredSession,
        p: &RefreshCtx,
        failure: RegisterFailure,
    ) -> BridgeResult<()> {
        self.registration_refresh = None;
        let busy = self.busy();
        self.last_refresh = Some((
            SystemTime::now(),
            match failure.status {
                Some(status) => format!("rejected {status}"),
                None if failure.error.to_string().contains("Timer F") => "timeout".to_string(),
                None => "error".to_string(),
            },
        ));
        let binding_valid = {
            let mut guard = p.status.lock().unwrap_or_else(|e| e.into_inner());
            guard.last_failure = Some((SystemTime::now(), failure.error.to_string()));
            let valid = guard.expires_at.is_some_and(|e| SystemTime::now() < e);
            if !valid {
                guard.state = crate::ims::RegistrationState::Failed;
            }
            valid
        };
        tracing::warn!(
            error = %failure.error,
            status = ?failure.status,
            in_call = busy,
            binding_valid,
            "registration refresh failed; keeping the existing session"
        );
        p.obs
            .report_registration_attempt(map_registration_error(&failure.error));
        p.obs.set_registered(binding_valid);

        if is_rejection(&failure) {
            self.refresh_rejections += 1;
            if self.refresh_rejections >= REJECTIONS_BEFORE_FALLBACK && !self.refresh_fallback {
                tracing::warn!(
                    rejections = self.refresh_rejections,
                    status = ?failure.status,
                    "in-place refresh keeps being rejected; the next cycle re-registers in full"
                );
                self.refresh_fallback = true;
            }
        }
        if needs_full_registration(failure.status) {
            self.force_renewal = true;
            return Ok(());
        }
        let target = session.pcscf_addr.ip();
        let routed = crate::ims::route_to(target, p.reg_cfg.pcscf_port).is_ok();
        let result = self.on_renewal_failure(target, &failure, routed, SystemTime::now());
        // The carrier's own pause is a floor under whatever the ladder chose
        // (RFC 3261 §20.33), unless the answer already sent us to another
        // P-CSCF at once.
        if let (Some(pause), false) = (failure.retry_after, self.force_renewal) {
            let floor = Instant::now() + pause.min(Duration::from_secs(86_400));
            self.next_renewal_attempt = self.next_renewal_attempt.max(Some(floor));
        }
        match result {
            // The exit would cut the call; it is owed until the line is idle.
            Err(e) if busy => {
                tracing::warn!(error = %e, "renewal budget spent mid-call; exiting once the call ends");
                self.budget_exit_owed = Some(e);
                Ok(())
            }
            result => result,
        }
    }
}

/// The two things a re-authentication needs from outside the process: the SIM
/// and the kernel's XFRM tables. Production uses the real ones; the tests
/// substitute a SIM that answers from a script and a table that follows the
/// kernel's rules, because CI has neither a modem nor `CAP_NET_ADMIN`. Nothing
/// else about the procedure is replaced.
#[derive(Clone)]
pub(super) struct ReauthEnv {
    pub(super) open_card: OpenCard,
    pub(super) xfrm: XfrmFactory,
}

pub(super) type OpenCard =
    Arc<dyn Fn(&ImsRegisterConfig) -> BridgeResult<crate::ims::OpenedCard> + Send + Sync>;
pub(super) type XfrmFactory = Arc<dyn Fn() -> Box<dyn XfrmBackend> + Send + Sync>;

impl Default for ReauthEnv {
    fn default() -> Self {
        Self {
            open_card: Arc::new(crate::ims::open_card),
            xfrm: Arc::new(|| Box::new(IpXfrm)),
        }
    }
}

/// How long a retired connection and its associations are kept after the
/// replacement took over: a transaction started on them (an outgoing call's
/// INVITE, say) can still be answered there (TS 33.203 §7.4.1a). The length of
/// a transaction, so nothing that was in flight at the switch outlives it.
const DRAIN: Duration = TRANSACTION_TIMEOUT;

/// No pending call can hold a retired leg longer than this.
const DRAIN_LIMIT: Duration = Duration::from_secs(600);

/// The SIM is answering a challenge. The registration is untouched meanwhile.
pub(super) struct Challenged {
    work: Reauthentication,
    carry: Carry,
    /// The `Security-Server` offers of the challenge, if it made any.
    offers: Vec<String>,
}

/// The client connection and associations a re-authentication replaced, kept
/// for the drain.
pub(super) struct RetiredLeg {
    transport: Option<SipTransport>,
    reader: Option<ClientReader>,
    sas: RetiredSas,
    /// When it may go, as long as nothing is waiting on it.
    due: Instant,
    /// When it goes regardless.
    limit: Instant,
}

impl RetiredLeg {
    fn close(&mut self) {
        if let Some(transport) = self.transport.take() {
            transport.abort();
        }
        if let Some(reader) = self.reader.take() {
            reader.stop();
            reader.reap();
        }
        self.sas.remove();
    }
}

impl Drop for RetiredLeg {
    fn drop(&mut self) {
        self.close();
    }
}

impl LoopState {
    /// The network answered a refresh with a challenge (TS 24.229 §5.1.1.5.1):
    /// hand it to the SIM on its own thread and carry on. Calls, notifications
    /// and the registration stay as they are until the answer is in.
    fn begin_challenge(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        mut pending: PendingRegister,
        response: &SipResponse,
    ) -> BridgeResult<bool> {
        let Some(www_authenticate) = response.header("WWW-Authenticate") else {
            self.fall_back_to_full("the challenge carried no WWW-Authenticate".into())?;
            return Ok(true);
        };
        if pending.kind == Kind::Resync && pending.resyncs >= MAX_RESYNCS {
            self.fall_back_to_full("the network kept asking for a resynchronisation".into())?;
            return Ok(true);
        }
        let job = ChallengeJob {
            cfg: p.reg_cfg.clone(),
            realm: session.realm.clone(),
            uri: format!(
                "sip:{}",
                register_request_uri(
                    session.pcscf_addr,
                    &session.realm,
                    p.reg_cfg.register_uri_home_domain
                )
            ),
            www_authenticate: www_authenticate.to_string(),
        };
        tracing::info!(
            in_call = self.busy(),
            "the network challenged the refresh; answering in the background"
        );
        self.challenged = Some(Challenged {
            work: {
                let open = Arc::clone(&self.reauth_env.open_card);
                Reauthentication::spawn(job, move |cfg| open(cfg))
            },
            offers: response
                .headers_all("Security-Server")
                .into_iter()
                .map(str::to_string)
                .collect(),
            carry: Carry {
                proposal: pending.proposal.take(),
                reserved: pending._reserved_port.take(),
                resyncs: pending.resyncs,
                requested_expires: pending.requested_expires,
            },
        });
        Ok(true)
    }

    /// Runs once per pass while the SIM works: takes the answer when it is in,
    /// and gives up on one that takes too long.
    fn drive_challenge(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
        now: Instant,
    ) -> BridgeResult<()> {
        let Some(challenged) = self.challenged.take() else {
            return Ok(());
        };
        let Some(result) = challenged.work.poll() else {
            if challenged.work.overdue(now) {
                return self
                    .fall_back_to_full("the SIM did not answer the challenge in time".into());
            }
            self.challenged = Some(challenged);
            return Ok(());
        };
        let Challenged {
            mut carry,
            offers,
            work: _,
        } = challenged;
        match result {
            Err(e) => {
                self.fall_back_to_full(format!("the SIM could not answer the challenge: {e}"))
            }
            Ok(Answer::Resynchronise { authorization }) => {
                let mut carry = carry;
                carry.resyncs += 1;
                match PendingRegister::launch(
                    session,
                    p.reg_cfg,
                    now,
                    carry,
                    Kind::Resync,
                    Some(authorization),
                    None,
                ) {
                    Ok(pending) => {
                        tracing::info!("asked the network to resynchronise");
                        self.registration_refresh = Some(pending);
                        Ok(())
                    }
                    Err(e) => {
                        self.fall_back_to_full(format!("could not send the resynchronisation: {e}"))
                    }
                }
            }
            Ok(Answer::Authenticated {
                authorization,
                ik,
                ck,
            }) => {
                let swap =
                    match self.stage_replacement(session, p, link, &mut carry, &offers, &ik, &ck) {
                        Ok(swap) => swap,
                        Err(e) => {
                            return self.fall_back_to_full(format!(
                                "could not set up the new Gm association: {e}"
                            ))
                        }
                    };
                // The port held back for the proposal is the replacement
                // connection's now (or no longer needed).
                let carry = Carry {
                    reserved: None,
                    ..carry
                };
                match PendingRegister::launch(
                    session,
                    p.reg_cfg,
                    now,
                    carry,
                    Kind::Answer,
                    Some(authorization),
                    swap,
                ) {
                    Ok(pending) => {
                        tracing::info!(
                            new_association = pending.swap.is_some(),
                            "answered the challenge"
                        );
                        self.registration_refresh = Some(pending);
                        Ok(())
                    }
                    Err(e) => self.fall_back_to_full(format!("could not send the answer: {e}")),
                }
            }
        }
    }

    /// Installs the associations the challenge proposed beside the working
    /// ones and opens the connection that will carry the answer (TS 33.203
    /// §7.4). `None` when there is nothing to replace: no association on this
    /// registration, or none offered, in which case the answer goes out on the
    /// working connection.
    #[allow(clippy::too_many_arguments)]
    fn stage_replacement(
        &mut self,
        session: &RegisteredSession,
        p: &RefreshCtx,
        link: &mut dyn GmLink,
        carry: &mut Carry,
        offers: &[String],
        ik: &[u8],
        ck: &[u8],
    ) -> BridgeResult<Option<Swap>> {
        let (Some(gm), Some(ours)) = (&session.gm_state, carry.proposal.clone()) else {
            return Ok(None);
        };
        let ours = &ours;
        if offers.is_empty() {
            return Ok(None);
        }
        let refs: Vec<&str> = offers.iter().map(String::as_str).collect();
        let (theirs, _) = gm_ipsec::select_security_server(
            &refs,
            p.reg_cfg.gm_auth_alg.as_deref(),
            p.reg_cfg.gm_cipher_alg.as_deref(),
        )?;
        let endpoints = GmEndpoints::new(
            gm.endpoints.local_c.ip(),
            gm.endpoints.remote_s.ip(),
            ours,
            &theirs,
        );
        let keys = SaKeys::derive(&theirs, ik, ck)?;
        let old = SaSet {
            endpoints: gm.endpoints.clone(),
            ours: gm.proposal.clone(),
            theirs: gm.theirs.clone(),
        };
        let new = SaSet {
            endpoints: endpoints.clone(),
            ours: ours.clone(),
            theirs: theirs.clone(),
        };
        let staged = StagedSas::stage((self.reauth_env.xfrm)(), &old, new, &keys)?;
        // The proposal held the client port; the replacement connection
        // binds it, so it has to be free first.
        let destination = SocketAddr::new(endpoints.remote_s.ip(), theirs.port_s);
        drop(carry.reserved.take());
        let transport = SipTransport::connect_from(ours.port_c, destination, session.use_tcp)?;
        let reader = link.read_connection(&transport)?;
        Ok(Some(Swap {
            staged: Some(staged),
            transport: Some(transport),
            reader: Some(reader),
            sa: Some(GmSa {
                endpoints,
                proposal: ours.clone(),
                theirs,
                security_verify: crate::ims::security_verify_echo(&refs),
            }),
        }))
    }

    /// The network confirmed the replacement: it becomes the registration's
    /// connection and associations, and what it replaced is kept for the drain.
    fn activate_swap(
        &mut self,
        session: &mut RegisteredSession,
        link: &mut dyn GmLink,
        swap: Swap,
    ) -> BridgeResult<()> {
        let (staged, transport, reader, sa) = swap.into_parts();
        let retired = staged.activate()?;
        let local = transport.local_addr()?;
        let old_transport = session.transport.replace(transport);
        session.local_addr = local;
        session.gm_state = Some(sa);
        let old_reader = link.adopt_reader(reader);
        let now = Instant::now();
        self.retired_gm.push(RetiredLeg {
            transport: old_transport,
            reader: Some(old_reader),
            sas: retired,
            due: now + DRAIN,
            limit: now + DRAIN_LIMIT,
        });
        // A probe in flight went out on the connection that was just retired.
        self.ping.reset();
        tracing::info!(
            local = %local,
            drain_secs = DRAIN.as_secs(),
            "switched to the new Gm association; the old one drains"
        );
        Ok(())
    }

    /// Closes retired connections whose drain is over. One is kept while an
    /// outgoing call is still being placed, which may be waiting on it, but not
    /// beyond [`DRAIN_LIMIT`].
    pub(super) fn drive_retired(&mut self, now: Instant, waiting: bool) {
        self.retired_gm.retain_mut(|leg| {
            let done = now >= leg.limit || (now >= leg.due && !waiting);
            if done {
                leg.close();
                tracing::info!("retired Gm association removed");
            }
            !done
        });
    }

    /// The procedure cannot continue: whatever was staged is undone and the
    /// registration is renewed in full, as soon as the line is idle. The
    /// binding the network already granted keeps serving calls until then.
    fn fall_back_to_full(&mut self, reason: String) -> BridgeResult<()> {
        self.registration_refresh = None;
        self.challenged = None;
        tracing::warn!(reason = %reason, "re-authentication failed; registering in full");
        self.last_refresh = Some((SystemTime::now(), "error".to_string()));
        self.force_renewal = true;
        Ok(())
    }
}

/// How many times a challenge may be answered with a resynchronisation
/// request, as at registration (TS 24.229 §5.1.1.5.1, RFC 3310 §3.3).
const MAX_RESYNCS: u32 = 2;

/// The failure for a final error answer to the refresh.
fn rejected(response: &SipResponse) -> RegisterFailure {
    RegisterFailure {
        error: BridgeError::Ims(format!(
            "registration refresh rejected: {} {}",
            response.status, response.reason
        )),
        status: Some(response.status),
        retry_after: crate::ims::retry_after(&response.headers),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::sip_client::SipTransport;
    use crate::ims::RegistrationAuth;
    use std::net::UdpSocket;

    const CACHED: &str = "Digest nonce=\"cached\",response=\"last-response\"";

    /// A UDP session against a loopback peer standing in for the P-CSCF. The
    /// peer is a real socket, so the tests read exactly what goes on the wire.
    fn fixture() -> (RegisteredSession, ImsRegisterConfig, UdpSocket) {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let transport = SipTransport::connect(peer.local_addr().unwrap(), false).unwrap();
        let local = transport.local_addr().unwrap();
        let mut session =
            crate::ims::test_session_with_sa(Some(transport), peer.local_addr().unwrap(), None);
        session.local_addr = local;
        session.contact_addr = local;
        session.use_tcp = false;
        session.registration_auth = Some(RegistrationAuth {
            authorization: CACHED.into(),
        });
        let cfg = ImsRegisterConfig {
            modem_port: "/must-not-open-modem".into(),
            pcsc_reader: false,
            pcscf_addr: peer.local_addr().unwrap().ip(),
            pcscf_port: peer.local_addr().unwrap().port(),
            mcc: "001".into(),
            mnc: "01".into(),
            imsi: None,
            imei: None,
            use_tcp: false,
            sec_agree: false,
            msisdn: None,
            access_network_info: "3GPP-WLAN".into(),
            register_uri_home_domain: true,
            gm_auth_alg: None,
            gm_cipher_alg: None,
        };
        (session, cfg, peer)
    }

    fn receive(peer: &UdpSocket) -> String {
        let mut bytes = [0; 8192];
        let size = peer.recv(&mut bytes).unwrap();
        String::from_utf8(bytes[..size].to_vec()).unwrap()
    }

    fn response(pending: &PendingRegister, status: u16) -> SipResponse {
        SipResponse {
            status,
            reason: "test".into(),
            headers: vec![
                ("Call-ID".into(), pending.call_id.clone()),
                ("CSeq".into(), format!("{} REGISTER", pending.cseq)),
                (
                    "Via".into(),
                    format!("SIP/2.0/UDP 127.0.0.1;branch={}", pending.branch),
                ),
            ],
            body: String::new(),
        }
    }

    fn with_header(mut response: SipResponse, name: &str, value: &str) -> SipResponse {
        response.headers.push((name.into(), value.into()));
        response
    }

    #[test]
    fn a_refresh_replays_the_cached_authorization_on_the_registration_identity() {
        let (mut session, cfg, peer) = fixture();
        session.cseq = 900; // calls and probes advance this; REGISTER must not follow.
        let call_id = session.call_id.clone();
        let cseq = session.registration_cseq;
        let local = session.local_addr;

        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let wire = receive(&peer);

        assert!(wire.starts_with("REGISTER sip:ims.example.test SIP/2.0\r\n"));
        assert!(wire.contains(&format!("Authorization: {CACHED}\r\n")));
        assert!(wire.contains(&format!("Call-ID: {call_id}\r\n")));
        assert!(wire.contains(&format!("CSeq: {cseq} REGISTER\r\n")));
        assert!(wire.contains("P-Access-Network-Info: 3GPP-WLAN\r\n"));
        assert!(
            !wire.contains("integrity-protected"),
            "the P-CSCF adds that parameter (TS 24.229 5.2.2.2), not the UE"
        );
        assert_eq!(session.registration_cseq, cseq + 1);
        assert_eq!(session.cseq, 900, "the shared request counter is untouched");
        assert_eq!(session.transport().unwrap().local_addr().unwrap(), local);
        assert!(pending.matches(&response(&pending, 200)));
    }

    #[test]
    fn consecutive_refreshes_use_consecutive_registration_cseqs() {
        let (mut session, cfg, peer) = fixture();
        let first = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        receive(&peer);
        let second = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        receive(&peer);
        assert_eq!(second.cseq, first.cseq + 1);
    }

    #[test]
    fn a_registration_without_cached_credentials_cannot_be_refreshed_in_place() {
        let (mut session, cfg, _peer) = fixture();
        session.registration_auth = None;
        assert!(PendingRegister::start(&mut session, &cfg, Instant::now()).is_err());
    }

    #[test]
    fn a_protected_refresh_keeps_the_server_port_and_proposes_a_new_client_port() {
        let (mut session, mut cfg, peer) = fixture();
        let protected = crate::ims::test_session_with_sa(
            None,
            peer.local_addr().unwrap(),
            Some("network-offer"),
        );
        session.gm_state = protected.gm_state;
        cfg.sec_agree = true;
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let wire = receive(&peer);
        assert!(wire.contains("Require: sec-agree\r\n"));
        assert!(wire.contains("Security-Verify: network-offer\r\n"));
        assert!(wire.contains("Security-Client: ipsec-3gpp"));

        let proposal = pending.proposal.as_ref().expect("a proposal was made");
        assert_eq!(
            proposal.port_s, 5063,
            "the protected server port is unchanged"
        );
        assert_ne!(
            proposal.port_c, 5062,
            "the client port is new (TS 33.203 7.4)"
        );
        assert_ne!(proposal.port_c, session.local_addr.port());
        assert!(wire.contains(&format!("port-c={}", proposal.port_c)));
        assert!(wire.contains("port-s=5063"));
        assert!(
            !wire.contains("spi-c=1;") && !wire.contains("spi-s=2;"),
            "fresh SPIs, not the live association's"
        );
    }

    #[test]
    fn the_proposed_client_port_stays_reserved_until_the_transaction_ends() {
        let (mut session, cfg, peer) = fixture();
        let protected = crate::ims::test_session_with_sa(
            None,
            peer.local_addr().unwrap(),
            Some("network-offer"),
        );
        session.gm_state = protected.gm_state;
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let port = pending.proposal.as_ref().unwrap().port_c;
        let taken = UdpSocket::bind(("127.0.0.1", port));
        assert!(taken.is_err(), "nothing else may bind the proposed port");
        drop(pending);
        assert!(
            UdpSocket::bind(("127.0.0.1", port)).is_ok(),
            "released afterwards"
        );
    }

    #[test]
    fn only_the_exact_transaction_matches() {
        let (mut session, cfg, _peer) = fixture();
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let good = response(&pending, 200);
        assert!(pending.matches(&good));

        let mut other_call = good.clone();
        other_call.headers[0].1 = "someone-else".into();
        assert!(!pending.matches(&other_call));

        let mut other_cseq = good.clone();
        other_cseq.headers[1].1 = format!("{} REGISTER", pending.cseq + 1);
        assert!(!pending.matches(&other_cseq));

        // An OPTIONS probe shares the registration Call-ID and may share the number.
        let mut options = good.clone();
        options.headers[1].1 = format!("{} OPTIONS", pending.cseq);
        assert!(!pending.matches(&options));

        let mut other_branch = good.clone();
        other_branch.headers[2].1 = "SIP/2.0/UDP 127.0.0.1;branch=z9hG4bKold".into();
        assert!(!pending.matches(&other_branch));

        // More than one branch parameter in the top Via is ambiguous.
        let mut two = good.clone();
        two.headers[2].1 = format!(
            "SIP/2.0/UDP 127.0.0.1;branch={};branch=z9hG4bKother",
            pending.branch
        );
        assert!(!pending.matches(&two));
    }

    #[test]
    fn udp_retransmits_the_identical_request_and_a_provisional_does_not_extend_timer_f() {
        let (mut session, cfg, peer) = fixture();
        let start = Instant::now();
        let mut pending = PendingRegister::start(&mut session, &cfg, start).unwrap();
        let sent = receive(&peer);

        assert!(pending
            .poll(&mut session, start + Duration::from_secs(1))
            .unwrap());
        assert!(
            peer.recv(&mut [0; 16]).is_err(),
            "nothing is retransmitted before T1"
        );

        assert!(pending.poll(&mut session, start + T1).unwrap());
        assert_eq!(
            receive(&peer),
            sent,
            "same branch and CSeq: one transaction"
        );

        pending.provisional();
        let deadline = start + TRANSACTION_TIMEOUT;
        assert!(pending
            .poll(&mut session, deadline - Duration::from_secs(1))
            .unwrap());
        assert!(
            !pending.poll(&mut session, deadline).unwrap(),
            "a provisional answer must not move Timer F"
        );
    }

    #[test]
    fn retransmission_backs_off_to_t2() {
        let (mut session, cfg, peer) = fixture();
        let start = Instant::now();
        let mut pending = PendingRegister::start(&mut session, &cfg, start).unwrap();
        receive(&peer);
        let mut at = start;
        let mut gaps = Vec::new();
        for _ in 0..5 {
            let next = pending.retransmit_at;
            gaps.push(next - at);
            at = next;
            assert!(pending.poll(&mut session, at).unwrap());
            receive(&peer);
        }
        assert_eq!(
            gaps.iter().map(Duration::as_secs).collect::<Vec<_>>(),
            [2, 4, 8, 16, 16]
        );
    }

    #[test]
    fn tcp_does_not_retransmit_but_timer_f_still_applies() {
        let (mut session, cfg, peer) = fixture();
        session.use_tcp = true;
        let start = Instant::now();
        let mut pending = PendingRegister::start(&mut session, &cfg, start).unwrap();
        receive(&peer);
        assert!(pending
            .poll(&mut session, start + Duration::from_secs(60))
            .unwrap());
        assert!(peer.recv(&mut [0; 16]).is_err());
        assert!(!pending
            .poll(&mut session, start + TRANSACTION_TIMEOUT)
            .unwrap());
    }

    #[test]
    fn interval_too_brief_retries_once_with_a_new_transaction() {
        let (mut session, cfg, peer) = fixture();
        let mut pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        receive(&peer);
        let first = pending.cseq;
        let brief = with_header(response(&pending, 423), "Min-Expires", "7200");

        assert!(pending
            .retry_min_expires(&brief, &mut session, &cfg, Instant::now())
            .unwrap());
        let wire = receive(&peer);
        assert!(wire.contains("Expires: 7200\r\n"));
        assert_eq!(pending.cseq, first + 1);
        assert!(
            !pending.matches(&brief),
            "the old transaction's answer is stale"
        );

        let again = with_header(response(&pending, 423), "Min-Expires", "9000");
        assert!(!pending
            .retry_min_expires(&again, &mut session, &cfg, Instant::now())
            .unwrap());
    }

    #[test]
    fn interval_too_brief_without_a_usable_minimum_is_not_retried() {
        let (mut session, cfg, peer) = fixture();
        let mut pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        receive(&peer);
        let none = response(&pending, 423);
        assert!(!pending
            .retry_min_expires(&none, &mut session, &cfg, Instant::now())
            .unwrap());
        let not_larger = with_header(response(&pending, 423), "Min-Expires", "3600");
        assert!(!pending
            .retry_min_expires(&not_larger, &mut session, &cfg, Instant::now())
            .unwrap());
    }

    fn lifetime_response(contacts: &[&str], expires: Option<&str>) -> SipResponse {
        let mut headers: Vec<(String, String)> = contacts
            .iter()
            .map(|value| ("Contact".to_string(), value.to_string()))
            .collect();
        if let Some(value) = expires {
            headers.push(("Expires".into(), value.into()));
        }
        SipResponse {
            status: 200,
            reason: "OK".into(),
            headers,
            body: String::new(),
        }
    }

    fn ours() -> SocketAddr {
        "10.0.0.2:5063".parse().unwrap()
    }

    #[test]
    fn the_lifetime_comes_from_our_contact_not_another_devices() {
        let response = lifetime_response(
            &[
                "<sip:other@10.0.0.9:5060;transport=tcp>;expires=60",
                "<sip:me@10.0.0.2:5063;transport=tcp>;expires=3000",
            ],
            Some("1800"),
        );
        assert_eq!(granted_lifetime(&response, ours(), 3600).unwrap(), 3000);
    }

    #[test]
    fn a_zero_lifetime_is_a_lost_binding() {
        let response = lifetime_response(&["<sip:me@10.0.0.2:5063>;expires=0"], None);
        assert_eq!(granted_lifetime(&response, ours(), 3600).unwrap(), 0);
    }

    #[test]
    fn the_response_expires_is_used_only_when_our_contact_has_none() {
        let response = lifetime_response(&["<sip:me@10.0.0.2:5063>"], Some("1200"));
        assert_eq!(granted_lifetime(&response, ours(), 3600).unwrap(), 1200);
        let bare = lifetime_response(&[], None);
        assert_eq!(granted_lifetime(&bare, ours(), 3600).unwrap(), 3600);
    }

    #[test]
    fn a_contact_list_without_our_binding_cannot_confirm_a_refresh() {
        let response = lifetime_response(&["<sip:other@10.0.0.9:5060>;expires=3600"], None);
        assert!(granted_lifetime(&response, ours(), 3600).is_err());
    }

    #[test]
    fn malformed_or_duplicate_lifetimes_are_failures_not_defaults() {
        let malformed = lifetime_response(&["<sip:me@10.0.0.2:5063>;expires=soon"], None);
        assert!(granted_lifetime(&malformed, ours(), 3600).is_err());
        let duplicate = lifetime_response(
            &[
                "<sip:me@10.0.0.2:5063>;expires=100",
                "<sip:me@10.0.0.2:5063>;expires=200",
            ],
            None,
        );
        assert!(granted_lifetime(&duplicate, ours(), 3600).is_err());
        let twice = lifetime_response(&["<sip:me@10.0.0.2:5063>;expires=1;expires=2"], None);
        assert!(granted_lifetime(&twice, ours(), 3600).is_err());
    }

    #[test]
    fn quoted_parameters_do_not_count_as_the_lifetime() {
        let response = lifetime_response(
            &["\"a;expires=9\" <sip:me@10.0.0.2:5063;expires=8>;expires=3000"],
            None,
        );
        assert_eq!(granted_lifetime(&response, ours(), 3600).unwrap(), 3000);
    }
}
