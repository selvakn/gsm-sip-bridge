//! A REGISTER transaction on the live connection. Its responses belong to the
//! dispatch loop's single reader, so calls continue while registration renews.

use crate::error::{BridgeError, BridgeResult};
use crate::ims::sip_client::{build_register, random_hex, RegisterRequest, SipResponse};
use crate::ims::{
    build_security_client_headers, register_request_uri, ImsRegisterConfig, RegisteredSession,
    SaProposal, DEFAULT_EXPIRES,
};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

// TS 24.229 table 7.7.1 overrides the base SIP defaults for IMS access.
pub(super) const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(128);
const T1: Duration = Duration::from_secs(2);
const T2: Duration = Duration::from_secs(16);

pub(super) struct PendingRegister {
    pub(super) call_id: String,
    pub(super) cseq: u32,
    branch: String,
    request: String,
    pub(super) proposal: Option<SaProposal>,
    pub(super) reserved_port: Option<Socket>,
    pub(super) requested_expires: u32,
    deadline: Instant,
    retransmit_at: Instant,
    retransmit_interval: Duration,
    use_tcp: bool,
    provisional: bool,
    interval_retry: bool,
}

impl PendingRegister {
    pub(super) fn start(
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
    ) -> BridgeResult<Self> {
        let (proposal, reserved_port) = match &session.gm_state {
            Some(gm) => {
                let socket = Socket::new(
                    Domain::for_address(session.local_addr),
                    if session.use_tcp {
                        Type::STREAM
                    } else {
                        Type::DGRAM
                    },
                    Some(if session.use_tcp {
                        Protocol::TCP
                    } else {
                        Protocol::UDP
                    }),
                )
                .map_err(|e| BridgeError::Ims(format!("reserve reauthentication port: {e}")))?;
                socket
                    .bind(&SocketAddr::new(session.local_addr.ip(), 0).into())
                    .map_err(|e| BridgeError::Ims(format!("bind reauthentication port: {e}")))?;
                let port = socket
                    .local_addr()
                    .ok()
                    .and_then(|a| a.as_socket())
                    .ok_or_else(|| {
                        BridgeError::Ims("reserved reauthentication socket has no address".into())
                    })?
                    .port();
                (
                    Some(SaProposal {
                        spi_c: rand::random::<u32>() | 1,
                        spi_s: rand::random::<u32>() | 1,
                        port_c: port,
                        port_s: gm.proposal.port_s,
                    }),
                    Some(socket),
                )
            }
            None => (None, None),
        };
        let mut pending = Self {
            call_id: session.call_id.clone(),
            cseq: 0,
            branch: String::new(),
            request: String::new(),
            proposal,
            reserved_port,
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

    fn send_new(
        &mut self,
        session: &mut RegisteredSession,
        cfg: &ImsRegisterConfig,
        now: Instant,
    ) -> BridgeResult<()> {
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
        if let Some(proposal) = &self.proposal {
            extra.extend([
                "Require: sec-agree".to_string(),
                "Proxy-Require: sec-agree".to_string(),
            ]);
            extra.extend(build_security_client_headers(proposal));
        }
        if let Some(verify) = session.security_verify() {
            extra.push(format!("Security-Verify: {verify}"));
        }
        self.request = build_register(&RegisterRequest {
            registrar_uri: &register_request_uri(
                session.pcscf_addr,
                &session.realm,
                cfg.register_uri_home_domain,
            ),
            public_uri: &session.public_uri,
            // TS 24.229 §5.1.1.4.2: for UDP the Via sent-by names
            // the protected server port; the dispatch queue reads both legs.
            local_addr: if session.use_tcp {
                session.local_addr
            } else {
                session.contact_addr
            },
            contact_addr: session.contact_addr,
            call_id: &self.call_id,
            from_tag: &session.from_tag,
            branch: &self.branch,
            cseq: self.cseq,
            expires: self.requested_expires,
            transport: if session.use_tcp { "TCP" } else { "UDP" },
            authorization: session
                .registration_auth
                .as_ref()
                .map(|auth| auth.authorization.as_str()),
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

    pub(super) fn matches(&self, response: &SipResponse) -> bool {
        response.header("Call-ID") == Some(self.call_id.as_str())
            && response.header("CSeq").is_some_and(|v| {
                let mut parts = v.split_whitespace();
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

    pub(super) fn provisional(&mut self) {
        self.provisional = true;
    }

    /// RFC 3261 timer E retransmits only UDP, timer F applies to either transport.
    /// `false` means timer F expired; a provisional response never restarts it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ims::sip_client::SipTransport;
    use std::net::UdpSocket;

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
        session.registration_auth = Some(crate::ims::registration_auth::RegistrationAuth {
            impi: "user@ims.example.test".into(),
            authorization: "Digest nonce=\"cached\",response=\"last-response\"".into(),
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

    #[test]
    fn live_refresh_keeps_binding_and_credentials_and_uses_single_existing_reader() {
        let (mut session, cfg, peer) = fixture();
        session.cseq = 900; // Other SIP dialogs must not affect REGISTER ordering.
        let local = session.local_addr;
        let call_id = session.call_id.clone();
        let cseq = session.registration_cseq;
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let wire = receive(&peer);
        assert!(wire.contains("Authorization: Digest nonce=\"cached\",response=\"last-response\""));
        assert!(wire.contains(&format!("Call-ID: {call_id}\r\n")));
        assert!(wire.contains(&format!("CSeq: {cseq} REGISTER\r\n")));
        assert_eq!(session.local_addr, local);
        assert_eq!(session.transport().unwrap().local_addr().unwrap(), local);
        assert_eq!(session.registration_cseq, cseq + 1);
        assert_eq!(session.cseq, 900);
        assert!(pending.matches(&response(&pending, 200)));
    }

    #[test]
    fn udp_refresh_advertises_stable_server_port_and_reserves_a_new_client_port() {
        let (mut session, mut cfg, peer) = fixture();
        let protected = crate::ims::test_session_with_sa(
            None,
            peer.local_addr().unwrap(),
            Some("network-offer"),
        );
        session.gm_state = protected.gm_state;
        session.contact_addr = SocketAddr::new(session.local_addr.ip(), 5063);
        cfg.sec_agree = true;
        let local = session.local_addr;
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        let wire = receive(&peer);
        assert!(wire.contains("Via: SIP/2.0/UDP 127.0.0.1:5063;"));
        assert!(wire.contains("Security-Verify: network-offer\r\n"));
        let proposal = pending.proposal.as_ref().unwrap();
        assert_eq!(proposal.port_s, 5063);
        assert_ne!(proposal.port_c, local.port());
        assert!(wire.contains(&format!("port-c={}", proposal.port_c)));
        assert_eq!(session.transport().unwrap().local_addr().unwrap(), local);
        assert_eq!(
            pending
                .reserved_port
                .as_ref()
                .unwrap()
                .local_addr()
                .unwrap()
                .as_socket()
                .unwrap()
                .port(),
            proposal.port_c
        );
    }

    #[test]
    fn unrelated_or_stale_responses_cannot_finish_register() {
        let (mut session, cfg, peer) = fixture();
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        receive(&peer);
        for (key, value) in [
            ("Call-ID", "other"),
            ("CSeq", "2 INVITE"),
            ("CSeq", "999 REGISTER"),
            ("Via", "SIP/2.0/UDP host;branch=other"),
        ] {
            let mut wrong = response(&pending, 200);
            wrong.headers.iter_mut().find(|(k, _)| k == key).unwrap().1 = value.into();
            assert!(!pending.matches(&wrong));
        }
    }

    #[test]
    fn response_matching_requires_one_branch_in_the_top_via() {
        let (mut session, cfg, peer) = fixture();
        let pending = PendingRegister::start(&mut session, &cfg, Instant::now()).unwrap();
        receive(&peer);
        let mut reply = response(&pending, 200);
        reply
            .headers
            .iter_mut()
            .find(|(name, _)| name == "Via")
            .unwrap()
            .1 = format!(
            "SIP/2.0/UDP 127.0.0.1;Branch = {} ;received=127.0.0.1",
            pending.branch
        );
        assert!(
            pending.matches(&reply),
            "parameter names and whitespace follow SIP grammar"
        );
        for via in [
            format!(
                "SIP/2.0/UDP proxy;branch=other, SIP/2.0/UDP host;branch={}",
                pending.branch
            ),
            format!("SIP/2.0/UDP host;branch=other;branch={}", pending.branch),
            format!("SIP/2.0/UDP host;branch={};branch=other", pending.branch),
            "SIP/2.0/UDP host;rport".to_string(),
        ] {
            reply
                .headers
                .iter_mut()
                .find(|(name, _)| name == "Via")
                .unwrap()
                .1 = via;
            assert!(!pending.matches(&reply));
        }
        for missing in ["Call-ID", "CSeq", "Via"] {
            let mut reply = response(&pending, 200);
            reply.headers.retain(|(name, _)| name != missing);
            assert!(!pending.matches(&reply));
        }
    }

    #[test]
    fn udp_retransmits_identical_transaction_and_provisional_does_not_extend_deadline() {
        let (mut session, cfg, peer) = fixture();
        let now = Instant::now();
        let mut pending = PendingRegister::start(&mut session, &cfg, now).unwrap();
        let first = receive(&peer);
        assert!(pending.poll(&mut session, now + T1).unwrap());
        assert_eq!(receive(&peer), first);
        pending.provisional();
        assert!(!pending
            .poll(&mut session, now + TRANSACTION_TIMEOUT)
            .unwrap());
    }

    #[test]
    fn interval_too_brief_retries_once_with_new_transaction() {
        let (mut session, cfg, peer) = fixture();
        let now = Instant::now();
        let mut pending = PendingRegister::start(&mut session, &cfg, now).unwrap();
        receive(&peer);
        let old = response(&pending, 200);
        let mut too_brief = response(&pending, 423);
        too_brief
            .headers
            .push(("Min-Expires".into(), "7200".into()));
        assert!(pending
            .retry_min_expires(&too_brief, &mut session, &cfg, now)
            .unwrap());
        assert!(receive(&peer).contains("Expires: 7200\r\n"));
        assert!(!pending.matches(&old));
        assert!(!pending
            .retry_min_expires(&too_brief, &mut session, &cfg, now)
            .unwrap());
    }

    #[test]
    fn lifetime_comes_from_our_contact_and_zero_is_a_lost_binding() {
        let contact = "127.0.0.1:5062".parse().unwrap();
        let mut response = SipResponse {
            status: 200,
            reason: "OK".into(),
            headers: vec![
                (
                    "Contact".into(),
                    "<sip:other@127.0.0.1:5064>;expires=10, <sip:us@127.0.0.1:5062>;expires=1200"
                        .into(),
                ),
                ("Expires".into(), "3600".into()),
            ],
            body: String::new(),
        };
        assert_eq!(granted_lifetime(&response, contact, 3600).unwrap(), 1200);
        response.headers[0].1 = "<sip:us@127.0.0.1:5062>;expires=0".into();
        assert_eq!(granted_lifetime(&response, contact, 3600).unwrap(), 0);
    }

    fn lifetime_response(contact: &str, expires: Option<&str>) -> SipResponse {
        let mut headers = vec![("Contact".into(), contact.into())];
        if let Some(expires) = expires {
            headers.push(("Expires".into(), expires.into()));
        }
        SipResponse {
            status: 200,
            reason: "OK".into(),
            headers,
            body: String::new(),
        }
    }

    #[test]
    fn lifetime_handles_quoted_names_and_ignores_uri_or_quoted_parameter_expiry() {
        let contact = "127.0.0.1:5062".parse().unwrap();
        let reply = lifetime_response(
            "\"Foreign, device\" <sip:other@127.0.0.1:5064>;expires=3, \"Our > device, line\" <SIP:us@127.0.0.1:5062;expires=1;transport=UDP>;+sip.instance=\"<urn:example;expires=0>\";EXPIRES=1200",
            Some("3600"),
        );
        assert_eq!(granted_lifetime(&reply, contact, 3600).unwrap(), 1200);
        let reply = lifetime_response("<sip:us@127.0.0.1:5062;expires=0>", Some("2400"));
        assert_eq!(granted_lifetime(&reply, contact, 3600).unwrap(), 2400);
    }

    #[test]
    fn lifetime_normalizes_ipv6_and_default_sip_ports() {
        let contact = "[2001:db8::1]:5062".parse().unwrap();
        let reply = lifetime_response(
            "<sip:us@[2001:0db8:0:0:0:0:0:1]:5062;transport=UDP>;expires=600",
            None,
        );
        assert_eq!(granted_lifetime(&reply, contact, 3600).unwrap(), 600);
        for (endpoint, header) in [
            ("127.0.0.1:5060", "sip:us@127.0.0.1;expires=1200"),
            ("[2001:db8::1]:5060", "<sip:us@[2001:db8::1]>;expires=1200"),
        ] {
            let reply = lifetime_response(header, None);
            assert_eq!(
                granted_lifetime(&reply, endpoint.parse().unwrap(), 3600).unwrap(),
                1200
            );
        }
    }

    #[test]
    fn lifetime_rejects_missing_binding_and_malformed_own_expiry() {
        let contact = "127.0.0.1:5062".parse().unwrap();
        for header in [
            "<sip:other@127.0.0.1:5064>;expires=1200",
            "<sips:us@127.0.0.1:5062>;expires=1200",
            "<sip:us@127.0.0.1:5062>;expires=bad",
            "<sip:us@127.0.0.1:5062>;expires=+1200",
            "<sip:us@127.0.0.1:5062>;expires=-1",
            "<sip:us@127.0.0.1:5062>;expires=4294967296",
            "<sip:us@127.0.0.1:5062>;expires=1200;expires=1800",
            "<sip:us@127.0.0.1:5062>;expires=1200;+sip.instance=\"unterminated",
            "<sip:us@127.0.0.1:5062>;expires=1200,<sip:us@127.0.0.1:5062>;expires=1200",
        ] {
            let reply = lifetime_response(header, Some("3600"));
            assert!(
                granted_lifetime(&reply, contact, 3600).is_err(),
                "accepted {header}"
            );
        }
    }

    #[test]
    fn expiry_fallback_is_used_only_when_own_contact_omits_it() {
        let contact = "127.0.0.1:5062".parse().unwrap();
        let reply = lifetime_response("<sip:us@127.0.0.1:5062>", Some("2400"));
        assert_eq!(granted_lifetime(&reply, contact, 3600).unwrap(), 2400);
        let reply = lifetime_response("<sip:us@127.0.0.1:5062>;expires=1200", Some("bad"));
        assert_eq!(granted_lifetime(&reply, contact, 3600).unwrap(), 1200);
        let reply = lifetime_response("<sip:us@127.0.0.1:5062>", Some("bad"));
        assert!(granted_lifetime(&reply, contact, 3600).is_err());
        let reply = lifetime_response("<sip:us@127.0.0.1:5062>", None);
        assert_eq!(granted_lifetime(&reply, contact, 3600).unwrap(), 3600);
    }
}
