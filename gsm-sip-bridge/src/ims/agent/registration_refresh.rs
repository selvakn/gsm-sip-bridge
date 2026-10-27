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
use crate::ims::observability::AgentObservability;
use crate::ims::session::RegisterFailure;
use crate::ims::sip_client::{build_register, random_hex, RegisterRequest, SipResponse};
use crate::ims::{
    build_security_client_headers, register_request_uri, ImsRegisterConfig, RegisteredSession,
    SaProposal, DEFAULT_EXPIRES,
};
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
        let mut pending = Self {
            call_id: session.call_id.clone(),
            cseq: 0,
            branch: String::new(),
            request: String::new(),
            requested_expires: DEFAULT_EXPIRES,
            deadline: now + TRANSACTION_TIMEOUT,
            retransmit_at: now + T1,
            retransmit_interval: T1,
            use_tcp: session.use_tcp,
            provisional: false,
            interval_retry: false,
        };
        pending.send_new(session, cfg, now)?;
        Ok(pending)
    }

    /// Builds and sends a new transaction (new branch, next REGISTER CSeq).
    fn send_new(
        &mut self,
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
    ) -> BridgeResult<()> {
        let authorization = session
            .registration_auth
            .as_ref()
            .map(|auth| auth.authorization.clone())
            .ok_or_else(|| BridgeError::Ims("cached authorization is gone".into()))?;
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
        if let Some(gm) = &session.gm_state {
            // The same security-agreement headers a UE sends on any
            // re-registration (RFC 3329, TS 24.229 §5.1.1.4.2). The proposal
            // keeps today's ports and takes fresh SPIs; if the network wants a
            // new association it challenges, and that is handled as a full
            // re-registration for now.
            let proposal = SaProposal {
                spi_c: rand::random::<u32>() | 1,
                spi_s: rand::random::<u32>() | 1,
                port_c: gm.proposal.port_c,
                port_s: gm.proposal.port_s,
            };
            extra.extend([
                "Require: sec-agree".to_string(),
                "Proxy-Require: sec-agree".to_string(),
            ]);
            extra.extend(build_security_client_headers(&proposal));
            extra.push(format!("Security-Verify: {}", gm.security_verify));
        }
        self.request = build_register(&RegisterRequest {
            registrar_uri: &register_request_uri(
                session.pcscf_addr,
                &session.realm,
                cfg.register_uri_home_domain,
            ),
            public_uri: &session.public_uri,
            local_addr: session.local_addr,
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
        session.transport_mut()?.send(&self.request)?;
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
            session.transport_mut()?.send(&self.request)?;
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
/// and 504; 401 is a fresh authentication challenge, handled as a full
/// re-registration until background re-authentication exists).
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
        queue: &std::sync::mpsc::Receiver<(
            crate::ims::sip_client::SipMessage,
            crate::ims::sip_client::SipSink,
        )>,
    ) -> BridgeResult<()> {
        if std::mem::take(&mut self.after_blocking_handler) {
            while let Ok(message) = queue.try_recv() {
                self.held.push_back(message);
            }
        }
        if self.held.is_empty() {
            self.drive_registration_refresh(session, p)?;
        }
        Ok(())
    }

    /// Runs once per dispatch pass: retransmits or times out the refresh in
    /// flight, or starts one that has fallen due.
    pub(super) fn drive_registration_refresh(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
    ) -> BridgeResult<()> {
        let now = Instant::now();
        if let Some(mut pending) = self.registration_refresh.take() {
            return match pending.poll(session, now) {
                Ok(true) => {
                    self.registration_refresh = Some(pending);
                    Ok(())
                }
                Ok(false) => self.fail_registration_refresh(
                    session,
                    p,
                    BridgeError::Ims("IMS registration refresh timed out (Timer F)".into()).into(),
                ),
                Err(e) => self.fail_registration_refresh(session, p, e.into()),
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
            Err(e) => self.fail_registration_refresh(session, p, e.into()),
        }
    }

    /// Consumes the answer to the refresh in flight. `Ok(false)` when
    /// `response` belongs to something else, so the caller keeps routing it.
    pub(super) fn handle_registration_answer(
        &mut self,
        session: &mut RegisteredSession,
        p: &RefreshCtx,
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
            200..=299 => return self.confirm_registration_refresh(session, p, &pending, response),
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
        pending: &PendingRegister,
        response: &SipResponse,
    ) -> BridgeResult<bool> {
        let granted =
            match granted_lifetime(response, session.contact_addr, pending.requested_expires) {
                Ok(0) => {
                    // Our binding is gone even though the request was answered.
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
        for (name, value) in &session.headers {
            let carried = ["Service-Route", "P-Associated-URI", "Path"]
                .iter()
                .any(|kept| name.eq_ignore_ascii_case(kept));
            if carried && !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
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
        match self.on_renewal_failure(target, &failure, routed, SystemTime::now()) {
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
    fn a_protected_refresh_proposes_the_current_ports_and_echoes_security_verify() {
        let (mut session, mut cfg, peer) = fixture();
        let protected = crate::ims::test_session_with_sa(
            None,
            peer.local_addr().unwrap(),
            Some("network-offer"),
        );
        session.gm_state = protected.gm_state;
        cfg.sec_agree = true;
        PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let wire = receive(&peer);
        assert!(wire.contains("Require: sec-agree\r\n"));
        assert!(wire.contains("Security-Verify: network-offer\r\n"));
        assert!(wire.contains("Security-Client: ipsec-3gpp"));
        assert!(wire.contains("port-c=5062") && wire.contains("port-s=5063"));
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
