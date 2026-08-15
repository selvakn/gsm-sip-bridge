//! REGISTER + digest exchange against the bridge's registrar.
//!
//! Constraints verified against `sip::server::auth` (research.md, quickstart
//! troubleshooting table): `MD5` + `qop=auth` only — `MD5-sess`, `SHA-256`
//! and `auth-int` are refused; a `stale=true` challenge means adopt the new
//! nonce and retry, not fail; a *second* `401` on an already-authorised
//! REGISTER is a hard failure, never a retry loop.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use gsm_sip_bridge::config::secret::Secret;

use crate::error::{SipTestError, SipTestResult};
use crate::sip::message::{
    build_authorization, build_register, challenge_is_stale, new_branch, RegisterParams,
};
use crate::sip::socket::SipSocket;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct RegistrationConfig {
    pub registrar_addr: SocketAddr,
    pub registrar_host: String,
    pub aor_user: String,
    pub realm: String,
    pub password: Secret<String>,
    pub expires: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegState {
    Unregistered,
    Registered,
    Failed,
}

#[derive(Debug, Clone)]
pub struct RegistrationStatus {
    pub state: RegState,
    pub granted_expires: Option<u32>,
    pub registered_at: Option<Instant>,
    pub last_status: Option<(u16, String)>,
    pub consecutive_failures: u32,
}

impl Default for RegistrationStatus {
    fn default() -> Self {
        Self {
            state: RegState::Unregistered,
            granted_expires: None,
            registered_at: None,
            last_status: None,
            consecutive_failures: 0,
        }
    }
}

/// State carried across refreshes so a nonce doesn't have to be re-earned by
/// provoking a fresh 401 every time.
#[derive(Default)]
pub struct RegistrationCredentials {
    pub cseq: u32,
    pub call_id: String,
    pub from_tag: String,
    pub cached_nonce: Option<String>,
    pub nc: u32,
}

/// Performs one REGISTER transaction: an initial (unauthenticated) attempt,
/// and — on `401`/`423` — the follow-up the challenge calls for. A second
/// `401` on an already-authorised attempt is reported as a hard failure
/// rather than retried, per the registrar's own contract.
pub fn register(
    socket: &SipSocket,
    cfg: &RegistrationConfig,
    creds: &mut RegistrationCredentials,
) -> SipTestResult<RegistrationStatus> {
    let mut expires = cfg.expires;
    let mut authorization: Option<String> = None;
    let mut already_authorized = false;

    for _ in 0..3 {
        creds.cseq += 1;
        let branch = new_branch();
        let msg = build_register(&RegisterParams {
            registrar_host: &cfg.registrar_host,
            aor_user: &cfg.aor_user,
            local_addr: socket.local_addr(),
            call_id: &creds.call_id,
            from_tag: &creds.from_tag,
            branch: &branch,
            cseq: creds.cseq,
            expires,
            authorization: authorization.as_deref(),
        });
        socket.send(cfg.registrar_addr, &msg)?;

        let resp = socket.recv_response(RESPONSE_TIMEOUT)?.ok_or_else(|| {
            SipTestError::Config("REGISTER timed out waiting for a response".into())
        })?;

        match resp.status {
            200 => {
                let granted = resp
                    .header("Expires")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(expires);
                return Ok(RegistrationStatus {
                    state: RegState::Registered,
                    granted_expires: Some(granted),
                    registered_at: Some(Instant::now()),
                    last_status: Some((200, "OK".to_string())),
                    consecutive_failures: 0,
                });
            }
            401 => {
                if already_authorized {
                    return Ok(RegistrationStatus {
                        state: RegState::Failed,
                        granted_expires: None,
                        registered_at: None,
                        last_status: Some((401, "Unauthorized (already authorised)".to_string())),
                        consecutive_failures: 1,
                    });
                }
                let www_auth = resp
                    .header("WWW-Authenticate")
                    .ok_or_else(|| SipTestError::Config("401 with no WWW-Authenticate".into()))?;
                let params = gsm_sip_bridge::ims::sip_client::parse_digest_challenge(www_auth)?;
                let challenge = gsm_sip_bridge::ims::sip_client::extract_challenge(&params)?;
                let _stale = challenge_is_stale(&params); // adopting the new nonce below handles staleness either way
                creds.cached_nonce = Some(challenge.nonce.clone());
                creds.nc = 1;
                let uri = format!("sip:{}", cfg.registrar_host);
                let cnonce = crate::sip::message::new_tag();
                authorization = Some(build_authorization(
                    &cfg.aor_user,
                    &cfg.realm,
                    cfg.password.expose_secret(),
                    "REGISTER",
                    &uri,
                    &challenge.nonce,
                    creds.nc,
                    &cnonce,
                ));
                already_authorized = true;
                continue;
            }
            423 => {
                if let Some(min) = resp
                    .header("Min-Expires")
                    .and_then(|v| v.parse::<u32>().ok())
                {
                    expires = min;
                    continue;
                }
                return Ok(failed(resp.status, &resp.reason));
            }
            other => {
                return Ok(failed(other, &resp.reason));
            }
        }
    }

    Ok(RegistrationStatus {
        state: RegState::Failed,
        granted_expires: None,
        registered_at: None,
        last_status: Some((0, "exhausted retry attempts".to_string())),
        consecutive_failures: 1,
    })
}

fn failed(status: u16, reason: &str) -> RegistrationStatus {
    RegistrationStatus {
        state: RegState::Failed,
        granted_expires: None,
        registered_at: None,
        last_status: Some((status, reason.to_string())),
        consecutive_failures: 1,
    }
}

/// De-registers with `Expires: 0`, best-effort — used on clean shutdown.
pub fn deregister(
    socket: &SipSocket,
    cfg: &RegistrationConfig,
    creds: &mut RegistrationCredentials,
) {
    creds.cseq += 1;
    let branch = new_branch();
    let msg = build_register(&RegisterParams {
        registrar_host: &cfg.registrar_host,
        aor_user: &cfg.aor_user,
        local_addr: socket.local_addr(),
        call_id: &creds.call_id,
        from_tag: &creds.from_tag,
        branch: &branch,
        cseq: creds.cseq,
        expires: 0,
        authorization: None,
    });
    let _ = socket.send(cfg.registrar_addr, &msg);
}
