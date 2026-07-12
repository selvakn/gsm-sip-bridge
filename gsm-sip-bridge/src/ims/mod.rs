//! IMS-AKA SIP REGISTER — an alternate mode alongside the existing GSM->SIP
//! voice flow, for registering to a mobile operator's IMS core over a
//! VoWiFi/ePDG tunnel (see `docker/epdg/`) using the SIM inside the modem.
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

pub mod call;
mod digest;
mod gm_ipsec;
mod rtp;
mod sdp;
mod sip_client;

use crate::error::{BridgeError, BridgeResult};
use crate::modules::at_commander::AtCommander;
use crate::modules::usim::{self, AkaResult};
use gm_ipsec::GmEndpoints;
use sip_client::{
    build_register, extract_challenge, format_sip_addr, parse_digest_challenge, random_hex,
    RegisterRequest, SipTransport,
};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

const DEFAULT_EXPIRES: u32 = 3600;
/// RFC 3310 §4.4: on a sync failure the client re-sends with an empty
/// password and an `auts` parameter; the server then issues a fresh
/// challenge. Cap resync attempts so a persistently out-of-sync SIM (or a
/// server that never accepts the resync) can't loop forever.
const MAX_RESYNC_ATTEMPTS: u32 = 2;

pub struct ImsRegisterConfig {
    pub modem_port: PathBuf,
    pub pcscf_addr: IpAddr,
    pub pcscf_port: u16,
    pub mcc: String,
    pub mnc: String,
    /// Overrides the IMSI read from the SIM via AT+CIMI, if set.
    pub imsi: Option<String>,
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
struct RegisteredSession {
    transport: SipTransport,
    realm: String,
    public_uri: String,
    local_addr: SocketAddr,
    use_tcp: bool,
    /// Next `CSeq` to use for a request on this session (already advanced
    /// past whatever REGISTER used).
    cseq: u32,
    gm_state: Option<(GmEndpoints, SaProposal, gm_ipsec::SecurityServerParams)>,
    xfrm_proto: &'static str,
    status: u16,
    reason: String,
    headers: Vec<(String, String)>,
}

impl RegisteredSession {
    /// Tear down any installed Gm IPsec state — a one-shot diagnostic CLI
    /// isn't a persistent registration, so kernel XFRM state would
    /// otherwise leak across repeated invocations.
    fn cleanup(&mut self) {
        if let Some((endpoints, p, theirs)) = self.gm_state.take() {
            gm_ipsec::remove_gm_sas(&endpoints, &p, &theirs, self.xfrm_proto);
        }
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

fn register_session(cfg: &ImsRegisterConfig) -> BridgeResult<RegisteredSession> {
    let mut at = AtCommander::open(&cfg.modem_port)?;

    let imsi = match &cfg.imsi {
        Some(imsi) => imsi.clone(),
        None => at.query_imsi()?,
    };
    tracing::info!(imsi = %imsi, "read IMSI from SIM");

    let aid = usim::discover_usim_aid(&mut at)?;
    usim::select_usim(&mut at, &aid)?;
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
    // PJSIP-based implementations (e.g. Asterisk's res_pjsip_outbound_registration,
    // via pjsip_regc_init's srv_url) set the REGISTER Request-URI to the
    // literal P-CSCF address from `server_uri`, not the home-network realm —
    // matching that is what gets past this network's registrar (a realm
    // Request-URI got an instant `406 User Unknown` on Airtel).
    let request_uri = format_sip_addr(pcscf);

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

    // Mandated by TS 24.229 for a WLAN-access (VoWiFi/SWu) REGISTER so the
    // P-CSCF can attribute the request to the right access leg; real UEs and
    // Asterisk's Gm transport both always send this.
    let mut extra_headers = vec!["P-Access-Network-Info: 3GPP-WLAN".to_string()];
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

        let aka = usim::authenticate(&mut at, &rand_arr, &autn_arr)?;

        cseq += 1;
        // RFC 2617 requires this to match the Request-URI of the message it's
        // attached to (it feeds into the HA2 digest and the server checks it).
        let uri = format!("sip:{request_uri}");
        let sec_server_hdr = resp.header("Security-Server").map(|s| s.to_string());
        let (auth_header, was_resync) = match aka {
            AkaResult::Success { res, ck, ik } => {
                tracing::info!("AKA success, building Authorization response");
                if let (Some(p), Some(sec_server)) = (proposal.as_ref(), sec_server_hdr.as_deref())
                {
                    match gm_ipsec::parse_security_server(sec_server) {
                        Ok(theirs) => {
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
                                            transport = Some(new_transport);
                                            tracing::info!(local = %local_addr, peer = %new_dst, "reconnected over Gm IPsec transport");
                                            // RFC 3329 §2.4: echo the network's own
                                            // Security-Server value back in a Security-Verify
                                            // header on the request sent over the now-selected
                                            // SA, confirming which negotiated association is in
                                            // use (a captured working Asterisk registration
                                            // always includes this on the post-IPsec retry).
                                            extra_headers
                                                .push(format!("Security-Verify: {sec_server}"));
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
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to parse Security-Server header")
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
            call_id: &call_id,
            from_tag: &from_tag,
            branch: &branch,
            cseq,
            expires: DEFAULT_EXPIRES,
            transport: via_transport,
            authorization: Some(&auth_header),
            extra_headers: &extra_headers,
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
        transport,
        realm,
        public_uri,
        local_addr,
        use_tcp: cfg.use_tcp,
        cseq: cseq + 1,
        gm_state,
        xfrm_proto,
        status: resp.status,
        reason: resp.reason,
        headers: resp.headers,
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

/// Build the `Supported: sec-agree` + `Security-Client: ipsec-3gpp` header
/// pair (RFC 3329 / TS 24.229 Annex H) that some networks require even to
/// get past an initial `421 Extension Required`.
///
/// The wire format here matches sysmocom's `volte.c` as actually captured
/// from a real `200 OK` registration on Airtel India, not the generic RFC
/// 3329 grammar: one `Security-Client` header whose value is a comma-joined
/// list of `ipsec-3gpp;alg=<alg>;ealg=<ealg>;spi-c=..;spi-s=..;port-c=..;
/// port-s=..` tuples — no spaces around `;`, no `prot=`/`mod=`/`q=`, one
/// tuple per integrity algorithm (`hmac-md5-96`/`hmac-sha-1-96`), each with
/// `ealg=null` (no ESP encryption, integrity only — what the captured
/// working REGISTER proposed).
fn build_security_client_headers(proposal: &SaProposal) -> Vec<String> {
    const ALGS: [&str; 2] = ["hmac-md5-96", "hmac-sha-1-96"];
    const EALGS: [&str; 1] = ["null"];

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
}
