//! Background IMS-AKA reauthentication without replacing the live SIP reader.
//!
//! The old connection remains owned by the agent's dispatcher. AUTS exchanges
//! write through its sink and receive correlated responses forwarded by that
//! dispatcher. Only the newly negotiated connection is read by this worker.

use super::gm_ipsec::{self, GmEndpoints};
use super::session::RegisterFailure;
use super::sip_client::{
    build_register, extract_challenge, parse_digest_challenge, random_hex, RegisterRequest,
    SipMessage, SipResponse, SipSink, SipTransport,
};
use super::{GmSa, ImsRegisterConfig, SaProposal, MAX_RESYNC_ATTEMPTS};
use crate::error::{BridgeError, BridgeResult};
use crate::modules::modem_lock::ModemLock;
use crate::modules::usim::{self, AkaResult, ApduTransport};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(128);
const READ_POLL: Duration = Duration::from_millis(100);
pub(crate) const WORK_TIMEOUT: Duration = Duration::from_secs(600);

/// Cache the successful AKA response for protected refreshes (TS 24.229
/// 5.1.1.4.2). Deliberately has no Debug implementation: it holds credentials.
pub(crate) struct RegistrationAuth {
    pub(crate) impi: String,
    pub(crate) authorization: String,
}

pub(crate) struct ReauthRequest {
    pub(crate) cfg: ImsRegisterConfig,
    pub(crate) modem_lock: Option<Arc<ModemLock>>,
    pub(crate) impi: String,
    pub(crate) public_uri: String,
    pub(crate) call_id: String,
    pub(crate) from_tag: String,
    pub(crate) cseq: u32,
    /// Published before each new REGISTER attempt so failed/cancelled workers
    /// cannot make the dispatcher's next transaction reuse its sequence.
    pub(crate) next_cseq: Arc<AtomicU32>,
    pub(crate) requested_expires: u32,
    pub(crate) contact_addr: SocketAddr,
    pub(crate) local_addr: SocketAddr,
    pub(crate) proposal: SaProposal,
    pub(crate) previous: GmSa,
    pub(crate) response: SipResponse,
    pub(crate) reserved_port: socket2::Socket,
    pub(crate) imei: String,
    pub(crate) old_transport: SipSink,
    pub(crate) old_responses: Receiver<SipResponse>,
    pub(crate) cancel: Arc<AtomicBool>,
}

/// A successfully authenticated replacement whose shared outbound policies
/// have not yet been activated. Dropping it before activation rolls back only
/// the staged SA, leaving the current call's security associations in place.
pub(crate) struct Reauthenticated {
    pub(crate) transport: SipTransport,
    pub(crate) gm: GmSa,
    pub(crate) auth: RegistrationAuth,
    pub(crate) response: SipResponse,
    pub(crate) next_cseq: u32,
    pub(crate) requested_expires: u32,
    pub(crate) reader: JoinHandle<()>,
    stage: StagedAssociation,
}

pub(crate) struct CommittedReauth {
    pub(crate) transport: SipTransport,
    pub(crate) gm: GmSa,
    pub(crate) auth: RegistrationAuth,
    pub(crate) response: SipResponse,
    pub(crate) next_cseq: u32,
    pub(crate) requested_expires: u32,
    pub(crate) reader: JoinHandle<()>,
}

impl Reauthenticated {
    pub(crate) fn commit(mut self) -> BridgeResult<CommittedReauth> {
        self.activate()?;
        Ok(CommittedReauth {
            transport: self.transport,
            gm: self.gm,
            auth: self.auth,
            response: self.response,
            next_cseq: self.next_cseq,
            requested_expires: self.requested_expires,
            reader: self.reader,
        })
    }

    pub(crate) fn activate(&mut self) -> BridgeResult<()> {
        self.stage
            .backend
            .activate(&self.gm, &self.stage.previous, self.stage.proto)?;
        self.stage.rollback = false;
        self.stage.abort_transport.take();
        Ok(())
    }
}

struct StagedAssociation {
    gm: GmSa,
    previous: GmSa,
    proto: &'static str,
    rollback: bool,
    abort_transport: Option<SipTransport>,
    stop: Arc<AtomicBool>,
    backend: Arc<dyn SaBackend>,
}

impl Drop for StagedAssociation {
    fn drop(&mut self) {
        if !self.rollback {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        if let Some(transport) = self.abort_transport.as_ref() {
            transport.shutdown();
        }
        self.backend.remove(&self.gm, &self.previous, self.proto);
    }
}

/// Kernel effects are isolated so an entire AKA/REGISTER/commit exchange can
/// be tested over loopback without installing privileged IPsec policies.
trait SaBackend: Send + Sync {
    fn install(
        &self,
        gm: &GmSa,
        previous: &GmSa,
        proto: &str,
        ik: &[u8],
        ck: &[u8],
    ) -> BridgeResult<()>;
    fn activate(&self, gm: &GmSa, previous: &GmSa, proto: &str) -> BridgeResult<()>;
    fn remove(&self, gm: &GmSa, previous: &GmSa, proto: &str);
}

struct KernelSaBackend;

impl SaBackend for KernelSaBackend {
    fn install(
        &self,
        gm: &GmSa,
        previous: &GmSa,
        proto: &str,
        ik: &[u8],
        ck: &[u8],
    ) -> BridgeResult<()> {
        gm_ipsec::install_gm_sas_for_reauth(
            &gm.endpoints,
            &gm.proposal,
            &gm.theirs,
            proto,
            ik,
            ck,
            &previous.endpoints,
            &previous.proposal,
            &previous.theirs,
        )
    }

    fn activate(&self, gm: &GmSa, previous: &GmSa, proto: &str) -> BridgeResult<()> {
        gm_ipsec::activate_gm_sas_after_reauth(
            &gm.endpoints,
            &gm.theirs,
            proto,
            &previous.endpoints,
            &previous.theirs,
        )
    }

    fn remove(&self, gm: &GmSa, previous: &GmSa, proto: &str) {
        gm_ipsec::remove_gm_sas_preserving(
            &gm.endpoints,
            &gm.proposal,
            &gm.theirs,
            proto,
            &previous.endpoints,
        );
    }
}

pub(crate) fn spawn(
    req: ReauthRequest,
    inbound_tx: Sender<(SipMessage, SipSink)>,
) -> Receiver<Result<Reauthenticated, RegisterFailure>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // If the agent has gone away, SendError drops a successful result and
        // its rollback guard, including its newly started reader.
        let _ = tx.send(run(req, inbound_tx));
    });
    rx
}

fn open_card(req: &ReauthRequest) -> BridgeResult<Box<dyn ApduTransport>> {
    let mut card: Box<dyn ApduTransport> = if req.cfg.pcsc_reader {
        let imsi = req
            .cfg
            .imsi
            .as_deref()
            .or_else(|| req.impi.split_once('@').map(|(imsi, _)| imsi))
            .ok_or_else(|| BridgeError::Ims("cached IMPI has no subscriber identity".into()))?;
        Box::new(crate::modules::pcsc_card::PcscTransport::connect(imsi)?)
    } else {
        Box::new(super::open_modem_waiting(
            &req.cfg.modem_port.to_string_lossy(),
            super::MODEM_OPEN_MAX_WAIT,
            super::MODEM_OPEN_RETRY_INTERVAL,
            || crate::modules::at_commander::AtCommander::open(&req.cfg.modem_port),
            std::thread::sleep,
        )?)
    };
    let aid = usim::discover_usim_aid(card.as_mut())?;
    usim::select_usim(card.as_mut(), &aid)?;
    Ok(card)
}

fn run(
    req: ReauthRequest,
    inbound_tx: Sender<(SipMessage, SipSink)>,
) -> Result<Reauthenticated, RegisterFailure> {
    run_with(req, inbound_tx, open_card, Arc::new(KernelSaBackend))
}

fn run_with(
    mut req: ReauthRequest,
    inbound_tx: Sender<(SipMessage, SipSink)>,
    open: impl FnOnce(&ReauthRequest) -> BridgeResult<Box<dyn ApduTransport>>,
    backend: Arc<dyn SaBackend>,
) -> Result<Reauthenticated, RegisterFailure> {
    let deadline = Instant::now() + WORK_TIMEOUT;
    check_work(deadline, &req.cancel)?;
    let lock = req.modem_lock.clone();
    let _guard = match lock.as_ref() {
        Some(lock) => Some(lock.lock().ok_or_else(|| {
            BridgeError::Ims(
                "could not get the modem for IMS reauthentication before timeout".into(),
            )
        })?),
        None => None,
    };
    check_work(deadline, &req.cancel)?;
    let mut card = open(&req)?;
    let realm = format!("ims.mnc{}.mcc{}.3gppnetwork.org", req.cfg.mnc, req.cfg.mcc);
    let pcscf = SocketAddr::new(req.cfg.pcscf_addr, req.cfg.pcscf_port);
    let registrar = super::register_request_uri(pcscf, &realm, req.cfg.register_uri_home_domain);
    let digest_uri = format!("sip:{registrar}");
    let mut response = req.response.clone();
    let mut resync_attempts = 0;
    let mut expires = req.requested_expires;
    let proto = if req.cfg.use_tcp { "tcp" } else { "udp" };

    loop {
        check_work(deadline, &req.cancel)?;
        let (challenge, aka) = authenticate_response(card.as_mut(), &response)?;
        match aka {
            AkaResult::SyncFailure { auts } => {
                resync_attempts += 1;
                if resync_attempts > MAX_RESYNC_ATTEMPTS {
                    return Err(BridgeError::Ims(
                        "AKA sync failure persisted past max resync attempts".into(),
                    )
                    .into());
                }
                // TS 24.229 5.1.1.5.3: new identifiers after synchronization
                // failure, with the existing protected server port retained.
                let reserved = reserve_client(req.local_addr, req.cfg.use_tcp)?;
                req.proposal.port_c = reserved
                    .local_addr()
                    .map_err(|e| BridgeError::Ims(format!("reserved port lookup failed: {e}")))?
                    .as_socket()
                    .ok_or_else(|| BridgeError::Ims("reserved port is not IP".into()))?
                    .port();
                req.proposal.spi_c = fresh_spi(req.proposal.spi_c, req.previous.proposal.spi_c);
                req.proposal.spi_s = fresh_spi(req.proposal.spi_s, req.previous.proposal.spi_s);
                req.reserved_port = reserved;
                let auth =
                    super::build_resync_authorization(&req.impi, &challenge, &digest_uri, &auts);
                let branch = format!("z9hG4bK{}", random_hex(6));
                let headers = request_headers(&req, &req.previous.security_verify);
                let request = register_request(
                    &req,
                    &registrar,
                    &branch,
                    expires,
                    &auth,
                    &headers,
                    req.local_addr,
                );
                req.next_cseq.store(next_cseq(req.cseq)?, Ordering::Release);
                response = exchange_old(
                    &req.old_transport,
                    &req.old_responses,
                    &req.call_id,
                    req.cseq,
                    &branch,
                    &request,
                    req.cfg.use_tcp,
                    deadline,
                    &req.cancel,
                )?;
                req.cseq = next_cseq(req.cseq)?;
                if response.status != 401 {
                    return Err(rejected(&response));
                }
                // Parse the newly returned challenge on the next iteration;
                // the RAND/AUTN that failed must never be retried after AUTS.
            }
            AkaResult::Success { res, ck, ik } => {
                // Network retries no longer need the SIM. Release the serial
                // port before any connect or REGISTER response wait.
                drop(card);
                drop(_guard);
                let offers = security_offers(&response);
                let (theirs, _) = gm_ipsec::select_security_server(
                    &offers,
                    req.cfg.gm_auth_alg.as_deref(),
                    req.cfg.gm_cipher_alg.as_deref(),
                )?;
                validate_replacement(&req, &theirs)?;
                let gm = GmSa {
                    endpoints: GmEndpoints::new(
                        req.local_addr.ip(),
                        pcscf.ip(),
                        &req.proposal,
                        &theirs,
                    ),
                    proposal: req.proposal.clone(),
                    theirs,
                    security_verify: super::security_verify_echo(&offers),
                };
                check_work(deadline, &req.cancel)?;
                backend.install(&gm, &req.previous, proto, &ik, &ck)?;
                let stop = Arc::new(AtomicBool::new(false));
                let mut stage = StagedAssociation {
                    gm: gm.clone(),
                    previous: req.previous.clone(),
                    proto,
                    rollback: true,
                    abort_transport: None,
                    stop: stop.clone(),
                    backend: backend.clone(),
                };
                check_work(deadline, &req.cancel)?;
                let dst = SocketAddr::new(pcscf.ip(), gm.theirs.port_s);
                let reserved = req
                    .reserved_port
                    .try_clone()
                    .map_err(|e| BridgeError::Ims(format!("reserved socket clone failed: {e}")))?;
                let mut transport = SipTransport::from_reserved(reserved, dst, req.cfg.use_tcp)?;
                transport.set_read_timeout(READ_POLL)?;
                stage.abort_transport = Some(transport.clone_for_writing()?);
                let auth = super::build_authorization(&req.impi, &challenge, &digest_uri, &res);
                let headers = request_headers(&req, &gm.security_verify);
                let local = transport.local_addr()?;
                let mut min_expires_retried = false;
                loop {
                    let branch = format!("z9hG4bK{}", random_hex(6));
                    let request = register_request(
                        &req, &registrar, &branch, expires, &auth, &headers, local,
                    );
                    req.next_cseq.store(next_cseq(req.cseq)?, Ordering::Release);
                    response = exchange_new(
                        &mut transport,
                        &inbound_tx,
                        &req.call_id,
                        req.cseq,
                        &branch,
                        &request,
                        req.cfg.use_tcp,
                        deadline,
                        &req.cancel,
                    )?;
                    req.cseq = next_cseq(req.cseq)?;
                    if response.status == 423 && !min_expires_retried {
                        expires = minimum_expires(&response, expires)?;
                        min_expires_retried = true;
                        continue;
                    }
                    break;
                }
                if response.status != 200 {
                    return Err(rejected(&response));
                }
                let writer = transport.clone_for_writing()?;
                let reader = spawn_reader(transport, inbound_tx, stop)?;
                return Ok(Reauthenticated {
                    transport: writer,
                    gm,
                    auth: RegistrationAuth {
                        impi: req.impi,
                        authorization: auth,
                    },
                    response,
                    next_cseq: req.cseq,
                    requested_expires: expires,
                    reader,
                    stage,
                });
            }
        }
    }
}

fn spawn_reader(
    mut transport: SipTransport,
    inbound_tx: Sender<(SipMessage, SipSink)>,
    stop: Arc<AtomicBool>,
) -> BridgeResult<JoinHandle<()>> {
    let sink = transport.sink()?;
    Ok(std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match transport.recv_message_deadline(READ_POLL) {
                Ok(Some(message)) => {
                    // Subsequent REGISTER responses also belong to the main
                    // dispatcher, which owns the next periodic refresh.
                    if inbound_tx.send((message, sink.clone())).is_err() {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "reauthenticated Gm reader stopped");
                    break;
                }
            }
        }
    }))
}

fn check_deadline(deadline: Instant) -> BridgeResult<()> {
    if Instant::now() >= deadline {
        Err(BridgeError::Ims(
            "timed out waiting for a SIP response during IMS reauthentication".into(),
        ))
    } else {
        Ok(())
    }
}

fn check_work(deadline: Instant, cancel: &AtomicBool) -> BridgeResult<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(BridgeError::Ims("IMS reauthentication cancelled".into()));
    }
    check_deadline(deadline)
}

fn next_cseq(cseq: u32) -> BridgeResult<u32> {
    cseq.checked_add(1)
        .filter(|next| *next < (1 << 31))
        .ok_or_else(|| BridgeError::Ims("REGISTER CSeq exhausted".into()))
}

fn fresh_spi(current: u32, previous: u32) -> u32 {
    loop {
        let spi: u32 = rand::random();
        if spi != 0 && spi != current && spi != previous {
            return spi;
        }
    }
}

fn reserve_client(local: SocketAddr, use_tcp: bool) -> BridgeResult<socket2::Socket> {
    let domain = if local.is_ipv6() {
        socket2::Domain::IPV6
    } else {
        socket2::Domain::IPV4
    };
    let kind = if use_tcp {
        socket2::Type::STREAM
    } else {
        socket2::Type::DGRAM
    };
    let socket = socket2::Socket::new(domain, kind, None)
        .map_err(|e| BridgeError::Ims(format!("reserve client socket failed: {e}")))?;
    socket
        .bind(&SocketAddr::new(local.ip(), 0).into())
        .map_err(|e| BridgeError::Ims(format!("reserve client port failed: {e}")))?;
    Ok(socket)
}

fn challenge_from(response: &SipResponse) -> BridgeResult<super::sip_client::DigestChallenge> {
    if response.status != 401 {
        return Err(BridgeError::Ims("expected IMS-AKA 401 challenge".into()));
    }
    let www = response
        .header("WWW-Authenticate")
        .ok_or_else(|| BridgeError::Ims("401 with no WWW-Authenticate header".into()))?;
    let challenge = extract_challenge(&parse_digest_challenge(www)?)?;
    if challenge.algorithm.as_deref() != Some("AKAv1-MD5") {
        return Err(BridgeError::Ims(
            "unsupported IMS reauthentication algorithm".into(),
        ));
    }
    Ok(challenge)
}

fn nonce_parts(nonce: &str) -> BridgeResult<([u8; 16], [u8; 16])> {
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, nonce)
        .map_err(|e| BridgeError::Ims(format!("nonce is not valid base64: {e}")))?;
    if decoded.len() < 32 {
        return Err(BridgeError::Ims("nonce too short for RAND+AUTN".into()));
    }
    let mut rand = [0; 16];
    let mut autn = [0; 16];
    rand.copy_from_slice(&decoded[..16]);
    autn.copy_from_slice(&decoded[16..32]);
    Ok((rand, autn))
}

fn authenticate_response(
    card: &mut dyn ApduTransport,
    response: &SipResponse,
) -> BridgeResult<(super::sip_client::DigestChallenge, AkaResult)> {
    let challenge = challenge_from(response)?;
    let (rand, autn) = nonce_parts(&challenge.nonce)?;
    let aka = usim::authenticate(card, &rand, &autn)?;
    Ok((challenge, aka))
}

fn security_offers(response: &SipResponse) -> Vec<&str> {
    response
        .headers_all("Security-Server")
        .into_iter()
        .flat_map(|value| value.split(',').map(str::trim))
        .collect()
}

fn validate_replacement(
    req: &ReauthRequest,
    theirs: &gm_ipsec::SecurityServerParams,
) -> BridgeResult<()> {
    if req.proposal.port_s != req.previous.proposal.port_s
        || theirs.port_s != req.previous.theirs.port_s
        || req.contact_addr != req.previous.endpoints.local_s
    {
        return Err(BridgeError::Ims(
            "reauthentication changed a protected server port".into(),
        ));
    }
    if req.proposal.port_c == req.previous.proposal.port_c
        || theirs.port_c == req.previous.theirs.port_c
        || req.proposal.spi_c == 0
        || req.proposal.spi_s == 0
        || req.proposal.spi_c == req.previous.proposal.spi_c
        || req.proposal.spi_s == req.previous.proposal.spi_s
    {
        return Err(BridgeError::Ims(
            "reauthentication did not provide fresh client ports and SPIs".into(),
        ));
    }
    Ok(())
}

fn request_headers(req: &ReauthRequest, security_verify: &str) -> Vec<String> {
    let mut headers = super::build_security_client_headers(&req.proposal);
    headers.push(format!(
        "P-Access-Network-Info: {}",
        req.cfg.access_network_info
    ));
    headers.push("Require: sec-agree".into());
    headers.push("Proxy-Require: sec-agree".into());
    headers.push(format!("Security-Verify: {security_verify}"));
    headers
}

fn register_request(
    req: &ReauthRequest,
    registrar: &str,
    branch: &str,
    expires: u32,
    auth: &str,
    headers: &[String],
    local: SocketAddr,
) -> String {
    build_register(&RegisterRequest {
        registrar_uri: registrar,
        public_uri: &req.public_uri,
        local_addr: if req.cfg.use_tcp {
            local
        } else {
            req.contact_addr
        },
        contact_addr: req.contact_addr,
        call_id: &req.call_id,
        from_tag: &req.from_tag,
        branch,
        cseq: req.cseq,
        expires,
        transport: if req.cfg.use_tcp { "TCP" } else { "UDP" },
        authorization: Some(auth),
        extra_headers: headers,
        imei: &req.imei,
    })
}

fn minimum_expires(response: &SipResponse, requested: u32) -> BridgeResult<u32> {
    response
        .header("Min-Expires")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|minimum| *minimum > requested)
        .ok_or_else(|| BridgeError::Ims("423 with no increasing Min-Expires".into()))
}

fn rejected(response: &SipResponse) -> RegisterFailure {
    RegisterFailure {
        error: BridgeError::Ims(format!(
            "reauthentication REGISTER rejected: {} {}",
            response.status, response.reason
        )),
        status: Some(response.status),
        retry_after: super::retry_after(&response.headers),
    }
}

fn matches_transaction(response: &SipResponse, call_id: &str, cseq: u32, branch: &str) -> bool {
    let Some(value) = response.header("CSeq") else {
        return false;
    };
    let mut parts = value.split_whitespace();
    if parts.next().and_then(|v| v.parse::<u32>().ok()) != Some(cseq)
        || parts.next() != Some("REGISTER")
        || parts.next().is_some()
        || response.header("Call-ID").map(str::trim) != Some(call_id)
    {
        return false;
    }
    response
        .header("Via")
        .and_then(|via| via.split(',').next())
        .is_some_and(|via| {
            let mut branches = via.split(';').skip(1).filter_map(|parameter| {
                let (name, value) = parameter.trim().split_once('=')?;
                name.eq_ignore_ascii_case("branch").then_some(value.trim())
            });
            branches.next() == Some(branch) && branches.next().is_none()
        })
}

fn is_register_response(message: &SipMessage) -> bool {
    matches!(message, SipMessage::Response(response)
        if response.header("CSeq").and_then(|v| v.split_whitespace().nth(1)) == Some("REGISTER"))
}

#[allow(clippy::too_many_arguments)]
fn exchange_old(
    sink: &SipSink,
    responses: &Receiver<SipResponse>,
    call_id: &str,
    cseq: u32,
    branch: &str,
    request: &str,
    use_tcp: bool,
    work_deadline: Instant,
    cancel: &AtomicBool,
) -> BridgeResult<SipResponse> {
    check_work(work_deadline, cancel)?;
    sink.send(request)?;
    let deadline = (Instant::now() + TRANSACTION_TIMEOUT).min(work_deadline);
    let mut interval = Duration::from_secs(2);
    let mut retransmit = Instant::now() + interval;
    loop {
        check_work(deadline, cancel)?;
        match responses
            .recv_timeout(READ_POLL.min(deadline.saturating_duration_since(Instant::now())))
        {
            Ok(response) if matches_transaction(&response, call_id, cseq, branch) => {
                if response.status >= 200 {
                    return Ok(response);
                }
                interval = Duration::from_secs(16);
                retransmit = Instant::now() + interval;
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(BridgeError::Ims(
                    "reauthentication dispatcher stopped".into(),
                ));
            }
        }
        if !use_tcp && Instant::now() >= retransmit {
            sink.send(request)?;
            interval = (interval * 2).min(Duration::from_secs(16));
            retransmit = Instant::now() + interval;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn exchange_new(
    transport: &mut SipTransport,
    inbound: &Sender<(SipMessage, SipSink)>,
    call_id: &str,
    cseq: u32,
    branch: &str,
    request: &str,
    use_tcp: bool,
    work_deadline: Instant,
    cancel: &AtomicBool,
) -> BridgeResult<SipResponse> {
    check_work(work_deadline, cancel)?;
    transport.send(request)?;
    let sink = transport.sink()?;
    let deadline = (Instant::now() + TRANSACTION_TIMEOUT).min(work_deadline);
    let mut interval = Duration::from_secs(2);
    let mut retransmit = Instant::now() + interval;
    loop {
        check_work(deadline, cancel)?;
        if let Some(message) = transport.recv_message_deadline(READ_POLL)? {
            match message {
                SipMessage::Response(response)
                    if matches_transaction(&response, call_id, cseq, branch) =>
                {
                    if response.status >= 200 {
                        return Ok(response);
                    }
                    interval = Duration::from_secs(16);
                    retransmit = Instant::now() + interval;
                }
                message if !is_register_response(&message) => {
                    inbound.send((message, sink.clone())).map_err(|_| {
                        BridgeError::Ims("reauthentication dispatcher stopped".into())
                    })?;
                }
                _ => {}
            }
        }
        if !use_tcp && Instant::now() >= retransmit {
            transport.send(request)?;
            interval = (interval * 2).min(Duration::from_secs(16));
            retransmit = Instant::now() + interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, UdpSocket};

    fn response(status: u16, branch: &str, cseq: u32, method: &str) -> SipResponse {
        SipResponse {
            status,
            reason: "Test".into(),
            body: String::new(),
            headers: vec![
                (
                    "Via".into(),
                    format!("SIP/2.0/TCP 127.0.0.1;branch={branch}"),
                ),
                ("Call-ID".into(), "register-id".into()),
                ("CSeq".into(), format!("{cseq} {method}")),
            ],
        }
    }

    fn wire_response(status: u16, branch: &str, cseq: u32, method: &str) -> String {
        format!("SIP/2.0 {status} Test\r\nVia: SIP/2.0/TCP 127.0.0.1;branch={branch}\r\nCall-ID: register-id\r\nCSeq: {cseq} {method}\r\nContent-Length: 0\r\n\r\n")
    }

    fn wire_request(method: &str) -> String {
        format!("{method} sip:user@example.net SIP/2.0\r\nVia: SIP/2.0/TCP 127.0.0.1;branch=call\r\nCall-ID: call-id\r\nCSeq: 9 {method}\r\nFrom: <sip:a@example.net>;tag=a\r\nTo: <sip:b@example.net>;tag=b\r\nContent-Length: 0\r\n\r\n")
    }

    #[test]
    fn transaction_requires_call_id_cseq_method_and_top_via_branch() {
        let good = response(200, "z9hG4bKgood", 42, "REGISTER");
        assert!(matches_transaction(&good, "register-id", 42, "z9hG4bKgood"));
        assert!(!matches_transaction(&good, "another-id", 42, "z9hG4bKgood"));
        assert!(!matches_transaction(
            &good,
            "register-id",
            41,
            "z9hG4bKgood"
        ));
        assert!(!matches_transaction(
            &good,
            "register-id",
            42,
            "z9hG4bKother"
        ));
        assert!(!matches_transaction(
            &response(200, "z9hG4bKgood", 42, "MESSAGE"),
            "register-id",
            42,
            "z9hG4bKgood"
        ));
    }

    #[test]
    fn new_tcp_connection_forwards_dialog_traffic_and_keeps_bytes_after_200() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut transport = SipTransport::connect(server.local_addr().unwrap(), true).unwrap();
        transport.set_read_timeout(READ_POLL).unwrap();
        let sender = std::thread::spawn(move || {
            let (mut peer, _) = server.accept().unwrap();
            assert_eq!(read_register(&mut peer).method, "REGISTER");
            // One TCP write deliberately includes unrelated responses, an
            // in-dialog BYE, the REGISTER result, and a following request.
            let messages = wire_response(200, "wrong", 42, "REGISTER")
                + &wire_response(200, "message", 20, "MESSAGE")
                + &wire_request("BYE")
                + &wire_response(200, "right", 42, "REGISTER")
                + &wire_request("OPTIONS")
                + &wire_response(200, "next-refresh", 43, "REGISTER");
            peer.write_all(messages.as_bytes()).unwrap();
        });
        let (tx, rx) = mpsc::channel();
        let answer = exchange_new(
            &mut transport,
            &tx,
            "register-id",
            42,
            "right",
            "REGISTER sip:x SIP/2.0\r\nContent-Length: 0\r\n\r\n",
            true,
            Instant::now() + Duration::from_secs(3),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(answer.status, 200);
        assert!(
            matches!(rx.recv().unwrap().0, SipMessage::Response(r) if r.header("CSeq") == Some("20 MESSAGE"))
        );
        assert!(matches!(rx.recv().unwrap().0, SipMessage::Request(r) if r.method == "BYE"));
        let _writer = transport.clone_for_writing().unwrap();
        let reader = spawn_reader(transport, tx, Arc::new(AtomicBool::new(false))).unwrap();
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(1)).unwrap().0, SipMessage::Request(r) if r.method == "OPTIONS")
        );
        assert!(
            matches!(rx.recv_timeout(Duration::from_secs(1)).unwrap().0, SipMessage::Response(r) if r.header("CSeq") == Some("43 REGISTER"))
        );
        reader.join().unwrap();
        sender.join().unwrap();
    }

    #[test]
    fn udp_retransmits_identical_register_until_matching_final_response() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut transport = SipTransport::connect(server.local_addr().unwrap(), false).unwrap();
        transport.set_read_timeout(READ_POLL).unwrap();
        let sender = std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            let (n, peer) = server.recv_from(&mut bytes).unwrap();
            let first = bytes[..n].to_vec();
            let (n, second_peer) = server.recv_from(&mut bytes).unwrap();
            assert_eq!(peer, second_peer);
            assert_eq!(first, bytes[..n]);
            server
                .send_to(wire_response(200, "right", 42, "REGISTER").as_bytes(), peer)
                .unwrap();
        });
        let (tx, _) = mpsc::channel();
        let answer = exchange_new(
            &mut transport,
            &tx,
            "register-id",
            42,
            "right",
            "REGISTER sip:x SIP/2.0\r\nContent-Length: 0\r\n\r\n",
            false,
            Instant::now() + Duration::from_secs(3),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(answer.status, 200);
        sender.join().unwrap();
    }

    #[test]
    fn old_connection_uses_dispatcher_responses_without_reading_its_socket() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let transport = SipTransport::connect(server.local_addr().unwrap(), false).unwrap();
        let sink = transport.sink().unwrap();
        let (tx, rx) = mpsc::channel();
        let dispatcher = std::thread::spawn(move || {
            let mut bytes = [0; 4096];
            server.recv_from(&mut bytes).unwrap();
            tx.send(response(401, "stale-branch", 42, "REGISTER"))
                .unwrap();
            tx.send(response(401, "fresh-branch", 42, "REGISTER"))
                .unwrap();
        });
        let answer = exchange_old(
            &sink,
            &rx,
            "register-id",
            42,
            "fresh-branch",
            "AUTS request",
            false,
            Instant::now() + Duration::from_secs(3),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            answer.header("Via"),
            Some("SIP/2.0/TCP 127.0.0.1;branch=fresh-branch")
        );
        dispatcher.join().unwrap();
    }

    #[test]
    fn fake_card_resynchronizes_then_authenticates_the_new_challenge() {
        struct Card {
            round: usize,
        }
        impl ApduTransport for Card {
            fn transmit_apdu(&mut self, apdu: &str) -> BridgeResult<String> {
                let octet = if self.round == 0 { "11" } else { "22" };
                assert_eq!(
                    apdu,
                    format!("008800812210{}10{}", octet.repeat(16), octet.repeat(16))
                );
                self.round += 1;
                Ok(if self.round == 1 {
                    format!("DC0E{}9000", "33".repeat(14))
                } else {
                    format!(
                        "DB08{}10{}10{}9000",
                        "44".repeat(8),
                        "55".repeat(16),
                        "66".repeat(16)
                    )
                })
            }
        }
        let mut card = Card { round: 0 };
        for (round, octet) in [0x11, 0x22].into_iter().enumerate() {
            let nonce =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [octet; 32]);
            let mut response = response(401, "right", 42 + round as u32, "REGISTER");
            response.headers.push((
                "WWW-Authenticate".into(),
                format!("Digest realm=\"ims.example.net\", nonce=\"{nonce}\", algorithm=AKAv1-MD5"),
            ));
            let (challenge, outcome) = authenticate_response(&mut card, &response).unwrap();
            assert_eq!(challenge.nonce, nonce);
            if round == 0 {
                assert!(matches!(outcome, AkaResult::SyncFailure { .. }));
            } else {
                assert!(matches!(outcome, AkaResult::Success { .. }));
            }
        }
        assert_eq!(card.round, 2);
    }

    #[test]
    fn interval_too_brief_requires_an_increasing_valid_minimum() {
        let mut response = response(423, "right", 42, "REGISTER");
        assert!(minimum_expires(&response, 3600).is_err());
        response.headers.push(("Min-Expires".into(), "3600".into()));
        assert!(minimum_expires(&response, 3600).is_err());
        response.headers.last_mut().unwrap().1 = "7200".into();
        assert_eq!(minimum_expires(&response, 3600).unwrap(), 7200);
    }

    #[test]
    fn cancelled_authentication_does_not_start_another_network_exchange() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut transport = SipTransport::connect(server.local_addr().unwrap(), false).unwrap();
        let (tx, _rx) = mpsc::channel();
        assert!(exchange_new(
            &mut transport,
            &tx,
            "register-id",
            42,
            "right",
            "REGISTER",
            false,
            Instant::now() + Duration::from_secs(3),
            &AtomicBool::new(true),
        )
        .is_err());
        assert!(server.recv_from(&mut [0; 4096]).is_err());
    }

    struct SuccessCard;
    impl ApduTransport for SuccessCard {
        fn transmit_apdu(&mut self, apdu: &str) -> BridgeResult<String> {
            assert_eq!(
                apdu,
                format!("008800812210{}10{}", "11".repeat(16), "11".repeat(16))
            );
            Ok(format!(
                "DB08{}10{}10{}9000",
                "44".repeat(8),
                "55".repeat(16),
                "66".repeat(16)
            ))
        }
    }

    struct FakeSaBackend {
        events: std::sync::Mutex<Vec<&'static str>>,
        fail_activation: bool,
        modem: Arc<ModemLock>,
    }

    impl SaBackend for FakeSaBackend {
        fn install(
            &self,
            gm: &GmSa,
            previous: &GmSa,
            _proto: &str,
            ik: &[u8],
            ck: &[u8],
        ) -> BridgeResult<()> {
            assert_eq!(gm.endpoints.local_s, previous.endpoints.local_s);
            assert_ne!(gm.endpoints.local_c, previous.endpoints.local_c);
            assert_eq!(ik, [0x66; 16]);
            assert_eq!(ck, [0x55; 16]);
            assert!(
                self.modem.lock_timeout(Duration::ZERO).is_some(),
                "SIM lock must be released before staging/network waits"
            );
            self.events.lock().unwrap().push("install");
            Ok(())
        }
        fn activate(&self, _gm: &GmSa, _previous: &GmSa, _proto: &str) -> BridgeResult<()> {
            self.events.lock().unwrap().push("activate");
            if self.fail_activation {
                Err(BridgeError::Ims("injected activation failure".into()))
            } else {
                Ok(())
            }
        }
        fn remove(&self, _gm: &GmSa, _previous: &GmSa, _proto: &str) {
            self.events.lock().unwrap().push("remove");
        }
    }

    fn read_register(peer: &mut std::net::TcpStream) -> super::super::sip_client::SipRequest {
        let mut data = Vec::new();
        loop {
            let mut part = [0; 4096];
            let read = peer.read(&mut part).unwrap();
            assert_ne!(read, 0);
            data.extend_from_slice(&part[..read]);
            if data.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        match super::super::sip_client::parse_datagram(std::str::from_utf8(&data).unwrap())
            .unwrap()
            .unwrap()
        {
            SipMessage::Request(request) => request,
            _ => panic!("expected REGISTER request"),
        }
    }

    type WorkerTestResult = (
        Result<Reauthenticated, RegisterFailure>,
        Arc<FakeSaBackend>,
        Receiver<(SipMessage, SipSink)>,
        u32,
    );

    fn run_fake(final_status: u16, retry_minimum: bool, fail_activation: bool) -> WorkerTestResult {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = server.local_addr().unwrap();
        let responder = std::thread::spawn(move || {
            let (mut peer, _) = server.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut request = read_register(&mut peer);
            assert_eq!(request.method, "REGISTER");
            assert!(request
                .header("Authorization")
                .unwrap()
                .contains("algorithm=AKAv1-MD5"));
            if retry_minimum {
                let response = format!("SIP/2.0 423 Interval Too Brief\r\nVia: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nMin-Expires: 7200\r\nContent-Length: 0\r\n\r\n", request.header("Via").unwrap(), request.header("Call-ID").unwrap(), request.header("CSeq").unwrap());
                peer.write_all(response.as_bytes()).unwrap();
                request = read_register(&mut peer);
                assert_eq!(request.header("Expires"), Some("7200"));
                assert_eq!(request.header("CSeq"), Some("43 REGISTER"));
            }
            let response = format!("SIP/2.0 {final_status} Test\r\nVia: {}\r\nCall-ID: {}\r\nCSeq: {}\r\nContent-Length: 0\r\n\r\n", request.header("Via").unwrap(), request.header("Call-ID").unwrap(), request.header("CSeq").unwrap());
            peer.write_all((response + &wire_request("OPTIONS")).as_bytes())
                .unwrap();
        });
        let old_server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let old_transport = SipTransport::connect(old_server.local_addr().unwrap(), false).unwrap();
        let local: SocketAddr = "127.0.0.1:10001".parse().unwrap();
        let reserved = reserve_client(local, true).unwrap();
        let local_port = reserved.local_addr().unwrap().as_socket().unwrap().port();
        let previous_proposal = SaProposal {
            spi_c: 10,
            spi_s: 11,
            port_c: 10001,
            port_s: 31001,
        };
        let previous_theirs = gm_ipsec::SecurityServerParams {
            alg: "hmac-md5-96".into(),
            ealg: "null".into(),
            spi_c: 20,
            spi_s: 21,
            port_c: 20001,
            port_s: server_addr.port(),
            q: 1.0,
        };
        let previous = GmSa {
            endpoints: GmEndpoints::new(
                local.ip(),
                server_addr.ip(),
                &previous_proposal,
                &previous_theirs,
            ),
            proposal: previous_proposal,
            theirs: previous_theirs,
            security_verify: "old-offer".into(),
        };
        let mut challenge = response(401, "initial", 41, "REGISTER");
        let nonce = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [0x11; 32]);
        challenge.headers.push((
            "WWW-Authenticate".into(),
            format!("Digest realm=\"ims.example.net\", nonce=\"{nonce}\", algorithm=AKAv1-MD5"),
        ));
        challenge.headers.push((
            "Security-Server".into(),
            format!(
                "ipsec-3gpp;alg=hmac-md5-96;ealg=null;spi-c=30;spi-s=31;port-c=20002;port-s={}",
                server_addr.port()
            ),
        ));
        let modem = Arc::new(ModemLock::new());
        let backend = Arc::new(FakeSaBackend {
            events: Default::default(),
            fail_activation,
            modem: modem.clone(),
        });
        let (_responses, old_responses) = mpsc::channel();
        let request = ReauthRequest {
            cfg: ImsRegisterConfig {
                modem_port: "/unused".into(),
                pcsc_reader: false,
                pcscf_addr: server_addr.ip(),
                pcscf_port: server_addr.port(),
                mcc: "001".into(),
                mnc: "001".into(),
                imsi: None,
                imei: None,
                use_tcp: true,
                sec_agree: true,
                msisdn: None,
                access_network_info: "3GPP-WLAN".into(),
                register_uri_home_domain: true,
                gm_auth_alg: None,
                gm_cipher_alg: None,
            },
            modem_lock: Some(modem),
            impi: "001001000000001@ims.example.net".into(),
            public_uri: "001001000000001@ims.example.net".into(),
            call_id: "register-id".into(),
            from_tag: "tag".into(),
            cseq: 42,
            next_cseq: Arc::new(AtomicU32::new(42)),
            requested_expires: 3600,
            contact_addr: previous.endpoints.local_s,
            local_addr: local,
            proposal: SaProposal {
                spi_c: 40,
                spi_s: 41,
                port_c: local_port,
                port_s: 31001,
            },
            previous,
            response: challenge,
            reserved_port: reserved,
            imei: "123456789012345".into(),
            old_transport: old_transport.sink().unwrap(),
            old_responses,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let (tx, rx) = mpsc::channel();
        let high_water = Arc::clone(&request.next_cseq);
        let result = run_with(
            request,
            tx,
            |_| Ok(Box::new(SuccessCard) as Box<dyn ApduTransport>),
            backend.clone(),
        );
        responder.join().unwrap();
        (result, backend, rx, high_water.load(Ordering::Acquire))
    }

    #[test]
    fn complete_reauthentication_commits_only_after_200_and_keeps_buffered_traffic() {
        let (result, backend, inbound, high_water) = run_fake(200, true, false);
        assert_eq!(high_water, 44);
        let replacement = result.unwrap();
        assert_eq!(*backend.events.lock().unwrap(), ["install"]);
        assert_eq!(replacement.requested_expires, 7200);
        let next = replacement.commit().unwrap();
        assert_eq!(next.next_cseq, 44);
        assert_eq!(next.requested_expires, 7200);
        assert_eq!(*backend.events.lock().unwrap(), ["install", "activate"]);
        assert!(
            matches!(inbound.recv_timeout(Duration::from_secs(1)).unwrap().0, SipMessage::Request(request) if request.method == "OPTIONS")
        );
        next.transport.shutdown();
        next.reader.join().unwrap();
        assert_eq!(
            *backend.events.lock().unwrap(),
            ["install", "activate"],
            "committed associations must not roll back when the result is dropped"
        );
    }

    #[test]
    fn abandoned_success_rolls_back_staged_associations_without_activation() {
        let (result, backend, _inbound, _) = run_fake(200, false, false);
        let replacement = result.unwrap();
        drop(replacement);
        assert_eq!(*backend.events.lock().unwrap(), ["install", "remove"]);
    }

    #[test]
    fn rejected_credentials_roll_back_staged_associations() {
        let (result, backend, _inbound, high_water) = run_fake(403, false, false);
        assert_eq!(
            high_water, 43,
            "a rejected REGISTER must still consume its CSeq"
        );
        assert!(matches!(
            result,
            Err(RegisterFailure {
                status: Some(403),
                ..
            })
        ));
        assert_eq!(*backend.events.lock().unwrap(), ["install", "remove"]);
    }

    #[test]
    fn failed_activation_rolls_back_staged_associations() {
        let (result, backend, _inbound, _) = run_fake(200, false, true);
        assert!(result.unwrap().commit().is_err());
        assert_eq!(
            *backend.events.lock().unwrap(),
            ["install", "activate", "remove"]
        );
    }
}
