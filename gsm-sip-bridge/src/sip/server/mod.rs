//! The embedded SIP registrar: IP phones REGISTER here and the bridge INVITEs
//! whichever one `[sip_server].ring_aor` names (spec 024).
//!
//! Pure safe Rust on its own UDP socket, deliberately *not* a PJSIP module
//! sharing pjsua's transport. A module would be nicer on the wire — the phone
//! would see INVITEs from the same port it registered to — but it would live
//! entirely behind the `pjsip-linked` feature, which neither `make test`,
//! `make lint`, nor CI ever enables. An authentication subsystem that is never
//! compiled or run in CI fails both of the constitution's non-negotiable
//! principles while appearing to satisfy them. See research.md R-001/R-002.
//!
//! Every request gets a definite response, including the ones we do not
//! support. Silently dropping a datagram makes a handset retransmit for 32
//! seconds, and an unanswered `OPTIONS` keepalive makes it mark us dead and
//! drop its registration — the mode would work, then quietly stop (R-006).

pub mod auth;
pub mod bindings;

pub use bindings::{Binding, BindingStore};

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::SipServerConfig;
use crate::ims::sip_client::{build_uas_response_with_headers, random_hex, SipRequest};

/// How long the socket blocks before the loop takes an idle tick. Short enough
/// that shutdown is prompt, long enough that an idle registrar is not spinning.
const READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Largest datagram we will look at. A REGISTER is a few hundred bytes;
/// anything near this is not a handset talking to us.
const MAX_DATAGRAM: usize = 8192;

/// What we advertise in `Allow`. Deliberately not `REGISTER`-only: a handset
/// reads this to decide what it may send us.
const ALLOW: &str = "INVITE, ACK, BYE, CANCEL, OPTIONS, REGISTER";

/// Called on the serve loop's idle tick with `(live_bindings,
/// ring_aor_is_registered)`.
///
/// Exists because the registrar's gauges are only scrapeable when the process
/// hosting it serves `/metrics` — true for the circuit-switched daemon, false
/// for the VoWiFi/VoLTE telephony agent. That host passes an observer which
/// forwards these to the daemon over the existing agent-reporting channel
/// instead (spec 024, FR-022).
pub type RegistrarObserver = Box<dyn Fn(u32, bool) + Send + Sync>;

/// A running registrar. Dropping it stops the thread.
pub struct Registrar {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    bindings: Arc<BindingStore>,
    local_addr: SocketAddr,
}

impl Registrar {
    /// Binds the socket and starts serving.
    ///
    /// Binding happens on the caller's thread so a port conflict is reported
    /// to whoever is starting the bridge, rather than disappearing into a
    /// worker that then silently serves nothing.
    pub fn start(config: &SipServerConfig) -> std::io::Result<Self> {
        Self::start_observed(config, None, None)
    }

    /// [`start`](Self::start) with an observer for hosts that cannot export the
    /// registrar's gauges themselves — see [`RegistrarObserver`].
    ///
    /// `outbound_local_port`: `Some(port)` when `[outbound].enabled` — a
    /// registered phone's INVITE is redirected to
    /// `sip:{aor}@{listen_addr}:{port}` (spec 025) instead of refused with
    /// `403`. `None` reproduces spec 024's behaviour exactly (FR-017).
    pub fn start_observed(
        config: &SipServerConfig,
        outbound_local_port: Option<u16>,
        observer: Option<RegistrarObserver>,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind((config.listen_addr.as_str(), config.listen_port))?;
        Self::start_on_observed(socket, config, outbound_local_port, observer)
    }

    /// [`start`](Self::start) on an already-bound socket. Tests bind
    /// `127.0.0.1:0` and read back [`local_addr`](Self::local_addr).
    pub fn start_on(socket: UdpSocket, config: &SipServerConfig) -> std::io::Result<Self> {
        Self::start_on_observed(socket, config, None, None)
    }

    /// [`start_on`](Self::start_on) with outbound calling enabled — see
    /// [`start_observed`](Self::start_observed)'s `outbound_local_port`.
    pub fn start_on_with_outbound(
        socket: UdpSocket,
        config: &SipServerConfig,
        outbound_local_port: u16,
    ) -> std::io::Result<Self> {
        Self::start_on_observed(socket, config, Some(outbound_local_port), None)
    }

    fn start_on_observed(
        socket: UdpSocket,
        config: &SipServerConfig,
        outbound_local_port: Option<u16>,
        observer: Option<RegistrarObserver>,
    ) -> std::io::Result<Self> {
        socket.set_read_timeout(Some(READ_TIMEOUT))?;
        let local_addr = socket.local_addr()?;

        let bindings = Arc::new(BindingStore::new());
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(ServerState {
            config: config.clone(),
            nonces: auth::NonceStore::new(Duration::from_secs(config.nonce_lifetime_sec)),
            bindings: Arc::clone(&bindings),
            outbound_local_port,
            observer,
        });

        tracing::info!(
            addr = %local_addr,
            realm = %config.realm,
            accounts = config.accounts.len(),
            ring_aor = %config.ring_aor,
            "sip_server: registrar listening"
        );

        let loop_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("sip-registrar".to_string())
            .spawn(move || serve(socket, state, loop_stop))?;

        Ok(Self {
            stop,
            handle: Some(handle),
            bindings,
            local_addr,
        })
    }

    /// The binding table, shared with whichever call path reads it.
    pub fn bindings(&self) -> Arc<BindingStore> {
        Arc::clone(&self.bindings)
    }

    /// The address actually bound — not always what was configured, since
    /// tests bind port 0 and let the kernel choose.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signals the serve loop and waits for it to finish.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::warn!("sip_server: registrar thread panicked during shutdown");
            }
        }
    }
}

impl Drop for Registrar {
    fn drop(&mut self) {
        self.stop();
    }
}

struct ServerState {
    config: SipServerConfig,
    nonces: auth::NonceStore,
    bindings: Arc<BindingStore>,
    /// `Some(port)` when `[outbound].enabled` — see `start_observed`.
    outbound_local_port: Option<u16>,
    observer: Option<RegistrarObserver>,
}

fn serve(socket: UdpSocket, state: Arc<ServerState>, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    while !stop.load(Ordering::SeqCst) {
        match socket.recv_from(&mut buf) {
            Ok((len, peer)) => {
                let now = Instant::now();
                let Ok(text) = std::str::from_utf8(&buf[..len]) else {
                    tracing::debug!(%peer, "sip_server: dropping non-UTF-8 datagram");
                    continue;
                };
                if let Some(response) = handle_datagram(text, peer, &state, now) {
                    if let Err(e) = socket.send_to(response.as_bytes(), peer) {
                        tracing::warn!(%peer, error = %e, "sip_server: failed to send response");
                    }
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Idle tick. Nothing depends on this — `get_live` filters
                // expired bindings anyway — but it keeps the tables and the
                // gauges from reporting phones that are long gone.
                let now = Instant::now();
                state.bindings.sweep(now);
                state.nonces.sweep(now);
                observe(&state, now);
            }
            Err(e) => {
                tracing::warn!(error = %e, "sip_server: recv failed");
            }
        }
    }
    tracing::info!("sip_server: registrar stopped");
}

/// Updates the registration gauges. Split out so the serve loop reads as
/// protocol handling rather than bookkeeping.
fn observe(state: &ServerState, now: Instant) {
    let live = state.bindings.live_count(now);
    let ringable = state
        .bindings
        .get_live(&state.config.ring_aor, now)
        .is_some();

    // Set locally regardless: correct and scrapeable when the host is the
    // daemon, harmless when it is not.
    crate::metrics::SIP_SERVER_BINDINGS.set(live as f64);
    crate::metrics::SIP_SERVER_RING_AOR_REGISTERED.set(if ringable { 1.0 } else { 0.0 });

    if let Some(observer) = &state.observer {
        observer(live as u32, ringable);
    }
}

/// Parses one datagram and produces the response to send back, if any.
///
/// Separate from the socket so the whole protocol surface is reachable from
/// tests without going through the network — though the integration tests do
/// go through a real socket anyway.
fn handle_datagram(
    text: &str,
    peer: SocketAddr,
    state: &ServerState,
    now: Instant,
) -> Option<String> {
    let request = match SipRequest::try_parse(text) {
        Ok(Some((request, _))) => request,
        // An incomplete or unparseable datagram is not a SIP request we can
        // even name, so there is nothing to answer — a response needs the
        // Via/From/Call-ID/CSeq we failed to read.
        Ok(None) | Err(_) => {
            tracing::debug!(%peer, "sip_server: dropping unparseable datagram");
            return None;
        }
    };

    let method = request.method.to_ascii_uppercase();
    let response = match method.as_str() {
        "REGISTER" => handle_register(&request, peer, state, now),
        // A handset keepalive. Left unanswered, Yealink and Grandstream mark
        // the server dead and drop their binding.
        "OPTIONS" => Response::new(200, "OK").with_header("Allow", ALLOW),
        // Phone-originated dialling is out of scope by default (spec 024
        // FR-020) — an explicit refusal beats a 32-second retransmit and a
        // timeout on the screen. Superseded when `[outbound].enabled`
        // (spec 025 FR-003): any *already-registered* phone (identified by
        // its REGISTERed source address, not a fresh digest exchange on
        // the INVITE itself) is redirected to the pjsua-hosted account
        // that can actually accept the call and place the mobile leg —
        // never any phone regardless of registration, which would let an
        // unauthenticated peer probe for a dial-out primitive.
        "INVITE" => match state.outbound_local_port {
            Some(local_port) => match state.bindings.find_by_source(peer, now) {
                Some(binding) => {
                    // A wildcard `listen_addr` (the default) means "every
                    // interface", not a routable host — same substitution
                    // `SipServerConfig::identity_uri` already applies to the
                    // ring target's own identity, reused here for the same
                    // reason (spec 024).
                    let host = match state.config.listen_addr.parse::<std::net::IpAddr>() {
                        Ok(ip) if ip.is_unspecified() => state.config.realm.as_str(),
                        _ => state.config.listen_addr.as_str(),
                    };
                    let contact = format!("sip:{}@{host}:{local_port}", binding.aor);
                    tracing::info!(%peer, aor = %binding.aor, %contact, "sip_server: redirecting a registered phone's dial-out attempt");
                    Response::new(302, "Moved Temporarily").with_contact(contact)
                }
                None => {
                    tracing::warn!(
                        %peer,
                        from = request.header("From").unwrap_or("<none>"),
                        "sip_server: refusing a call from an unregistered peer"
                    );
                    Response::new(403, "Forbidden")
                }
            },
            None => {
                tracing::warn!(
                    %peer,
                    from = request.header("From").unwrap_or("<none>"),
                    "sip_server: refusing a call from a phone — outbound calling is \
                     not enabled on this deployment"
                );
                Response::new(403, "Forbidden")
            }
        },
        "SUBSCRIBE" => Response::new(489, "Bad Event"),
        // ACK is hop-by-hop for a 4xx we sent; it is never answered.
        "ACK" => return None,
        _ => Response::new(405, "Method Not Allowed").with_header("Allow", ALLOW),
    };

    crate::metrics::SIP_SERVER_REQUESTS_TOTAL
        .with_label_values(&[&method, &response.status.to_string()])
        .inc();
    Some(response.render(&request))
}

/// A response under construction: status plus any headers the generic builder
/// cannot express positionally.
struct Response {
    status: u16,
    reason: &'static str,
    contact: Option<String>,
    extra: Vec<(String, String)>,
}

impl Response {
    fn new(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            contact: None,
            extra: Vec::new(),
        }
    }

    fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.extra.push((name.to_string(), value.into()));
        self
    }

    fn with_contact(mut self, contact: impl Into<String>) -> Self {
        self.contact = Some(contact.into());
        self
    }

    fn render(&self, request: &SipRequest) -> String {
        let extra: Vec<(&str, &str)> = self
            .extra
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        build_uas_response_with_headers(
            self.status,
            self.reason,
            request,
            Some(&to_tag(request)),
            self.contact.as_deref(),
            None,
            &extra,
        )
    }
}

/// A `To` tag for our response. `build_uas_response_with_headers` only applies
/// it when the request's `To` had none, so a stable-per-response random value
/// is fine and avoids inventing dialog state a registrar does not keep.
fn to_tag(_request: &SipRequest) -> String {
    random_hex(6)
}

fn handle_register(
    request: &SipRequest,
    peer: SocketAddr,
    state: &ServerState,
    now: Instant,
) -> Response {
    let config = &state.config;

    // Everything a response needs. Their absence is the one case we answer
    // with 400 rather than dropping, since we have enough to reply.
    let (Some(call_id), Some(cseq_header)) = (request.header("Call-ID"), request.header("CSeq"))
    else {
        return Response::new(400, "Bad Request");
    };
    let Some(cseq) = cseq_header
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return Response::new(400, "Bad Request");
    };

    // Retransmission, handled *before* authentication and deliberately so.
    //
    // Re-answering a request we already accepted is a transaction-layer
    // concern (RFC 3261 §17.2.1), which sits beneath authentication. Checking
    // credentials first would trip the nonce-count replay guard on the phone's
    // own retransmit — the exact datagram it re-sends when our response is
    // lost — and bounce a correctly-registered handset back to a 401. Nothing
    // is modified here, so replaying the current state is safe: the Contact it
    // reports came from the request being retransmitted.
    if let Some(existing) = matching_retransmission(request, call_id, cseq, &state.bindings) {
        let remaining = existing
            .expires_at
            .saturating_duration_since(now)
            .as_secs()
            .max(1);
        return Response::new(200, "OK")
            .with_contact(format!("<{}>;expires={remaining}", existing.contact_uri));
    }

    // Challenge first, always. One nonce, issued here, used once.
    let Some(authorization) = request.header("Authorization") else {
        crate::metrics::SIP_SERVER_REGISTRATIONS_TOTAL
            .with_label_values(&["challenged"])
            .inc();
        return challenge(state, now, false);
    };

    let username = match auth::verify(
        authorization,
        "REGISTER",
        &request.request_uri,
        &config.realm,
        &state.nonces,
        now,
        |u| config.password_for(u).map(str::to_string),
    ) {
        Ok(username) => username,
        Err(failure) => {
            crate::metrics::SIP_SERVER_REGISTRATIONS_TOTAL
                .with_label_values(&[failure.metric_label()])
                .inc();
            tracing::warn!(
                %peer,
                reason = ?failure,
                "sip_server: refusing a registration"
            );
            return challenge(state, now, failure.is_stale());
        }
    };

    let Some(contact_header) = request.header("Contact") else {
        return Response::new(400, "Bad Request");
    };

    let requested = requested_expires(request, contact_header);

    // De-registration. Still requires valid credentials, which is why it is
    // handled after the check above and not before it.
    if requested == Some(0) {
        state.bindings.remove(&username);
        crate::metrics::SIP_SERVER_REGISTRATIONS_TOTAL
            .with_label_values(&["deregistered"])
            .inc();
        tracing::info!(aor = %username, %peer, "sip_server: de-registered");
        observe(state, now);
        return Response::new(200, "OK");
    }

    // `Contact: *` is only meaningful with `Expires: 0`, handled above.
    if contact_header.trim() == "*" {
        return Response::new(400, "Bad Request");
    }
    let Some(contact_uri) = parse_contact_uri(contact_header) else {
        return Response::new(400, "Bad Request");
    };

    let granted = match requested {
        // Too short: tell the phone the floor rather than granting more than
        // it asked for and letting it believe the shorter value.
        Some(want) if want < config.min_expires => {
            crate::metrics::SIP_SERVER_REGISTRATIONS_TOTAL
                .with_label_values(&["rejected_interval"])
                .inc();
            return Response::new(423, "Interval Too Brief")
                .with_header("Min-Expires", config.min_expires.to_string());
        }
        Some(want) => want.min(config.max_expires),
        None => config.max_expires,
    };

    // Dialled verbatim. Rewriting it to `peer` would break handsets that
    // listen on a port other than the one they send from, so the mismatch is
    // reported rather than corrected.
    if let Some(host) = uri_host(&contact_uri) {
        if !peer.ip().to_string().eq_ignore_ascii_case(host) {
            tracing::warn!(
                aor = %username,
                contact_host = host,
                source = %peer,
                "sip_server: Contact host differs from the source address; \
                 dialling the Contact as given"
            );
        }
    }

    state.bindings.upsert(Binding {
        aor: username.clone(),
        contact_uri: contact_uri.clone(),
        source: peer,
        call_id: call_id.to_string(),
        cseq,
        expires_at: now + Duration::from_secs(u64::from(granted)),
        user_agent: request.header("User-Agent").map(str::to_string),
    });

    crate::metrics::SIP_SERVER_REGISTRATIONS_TOTAL
        .with_label_values(&["accepted"])
        .inc();
    tracing::info!(
        aor = %username,
        contact = %contact_uri,
        %peer,
        expires = granted,
        "sip_server: registered"
    );
    observe(state, now);

    Response::new(200, "OK").with_contact(format!("<{contact_uri}>;expires={granted}"))
}

/// The binding this REGISTER is a retransmission of, if any.
///
/// A retransmission is the same dialog (`Call-ID`) with no forward progress
/// (`CSeq` not advanced). We search by `Call-ID` rather than by account,
/// because at this point the request has not been authenticated and so there
/// is no account name to look under yet.
///
/// RFC 3261 §10.3 step 6 prefers `500 Server Error` for a *lower* `CSeq` on the
/// same `Call-ID`. Reporting current state with a `200 OK` is friendlier to
/// handsets and equally safe, since nothing is modified. Deliberate deviation.
fn matching_retransmission(
    request: &SipRequest,
    call_id: &str,
    cseq: u32,
    bindings: &BindingStore,
) -> Option<Binding> {
    let existing = bindings.find_by_call_id(call_id)?;
    if cseq > existing.cseq {
        return None;
    }
    // A `Contact` that differs is a *new* registration reusing the dialog, not
    // a retransmission — it must go through authentication like any other.
    let contact = request.header("Contact").and_then(parse_contact_uri)?;
    (contact == existing.contact_uri).then_some(existing)
}

fn challenge(state: &ServerState, now: Instant, stale: bool) -> Response {
    let nonce = state.nonces.issue(now);
    Response::new(401, "Unauthorized").with_header(
        "WWW-Authenticate",
        auth::challenge_header(&state.config.realm, &nonce, stale),
    )
}

/// The lifetime the phone asked for: the `Expires` header, or a `;expires=`
/// parameter on the `Contact`, or nothing (meaning "you choose").
fn requested_expires(request: &SipRequest, contact_header: &str) -> Option<u32> {
    if let Some(value) = request.header("Expires") {
        return value.trim().parse::<u32>().ok();
    }
    contact_header
        .split(';')
        .skip(1)
        .find_map(|p| p.trim().strip_prefix("expires="))
        .and_then(|v| v.trim().parse::<u32>().ok())
}

/// The bare URI out of a `Contact` value, dropping any display name and
/// parameters: `"Phone" <sip:1001@host:5060>;expires=60` -> `sip:1001@host:5060`.
fn parse_contact_uri(contact: &str) -> Option<String> {
    let trimmed = contact.trim();
    let uri = if let Some(start) = trimmed.find('<') {
        let end = trimmed[start..].find('>')? + start;
        &trimmed[start + 1..end]
    } else {
        // No angle brackets: parameters belong to the header, so stop at the
        // first `;` — everything after it is a header parameter, not part of
        // the URI.
        trimmed.split(';').next()?.trim()
    };
    let uri = uri.trim();
    if uri.is_empty() || !uri.to_ascii_lowercase().starts_with("sip") {
        return None;
    }
    Some(uri.to_string())
}

/// The host out of a SIP URI, for the source-address comparison.
fn uri_host(uri: &str) -> Option<&str> {
    let after_scheme = uri.split_once(':')?.1;
    let hostport = match after_scheme.rsplit_once('@') {
        Some((_, host)) => host,
        None => after_scheme,
    };
    let host = hostport.split(';').next()?;
    // Strip a port, but not the colons inside a bracketed IPv6 literal.
    let host = if host.starts_with('[') {
        host.split(']').next()?.trim_start_matches('[')
    } else {
        host.split(':').next()?
    };
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_contact_uri_is_extracted_from_every_form_handsets_send() {
        for (input, want) in [
            ("<sip:1001@192.168.1.50:5060>", "sip:1001@192.168.1.50:5060"),
            (
                "\"Desk\" <sip:1001@192.168.1.50:5060>;expires=60",
                "sip:1001@192.168.1.50:5060",
            ),
            ("sip:1001@192.168.1.50:5060", "sip:1001@192.168.1.50:5060"),
            (
                "sip:1001@192.168.1.50:5060;transport=udp",
                "sip:1001@192.168.1.50:5060",
            ),
            ("<sips:1001@host>", "sips:1001@host"),
        ] {
            assert_eq!(parse_contact_uri(input).as_deref(), Some(want), "{input}");
        }
    }

    #[test]
    fn a_contact_that_is_not_a_sip_uri_is_refused() {
        for input in ["", "*", "<>", "<tel:+15551234>", "<http://example.com>"] {
            assert_eq!(parse_contact_uri(input), None, "{input}");
        }
    }

    #[test]
    fn a_uri_host_is_extracted_without_the_port() {
        for (uri, want) in [
            ("sip:1001@192.168.1.50:5060", "192.168.1.50"),
            ("sip:1001@192.168.1.50", "192.168.1.50"),
            ("sip:192.168.1.50:5060", "192.168.1.50"),
            ("sip:1001@host;transport=udp", "host"),
            ("sip:1001@[2001:db8::1]:5060", "2001:db8::1"),
        ] {
            assert_eq!(uri_host(uri), Some(want), "{uri}");
        }
    }

    fn request_with(headers: &str) -> SipRequest {
        let raw = format!(
            "REGISTER sip:bridge SIP/2.0\r\n\
             Via: SIP/2.0/UDP 192.168.1.50:5060;branch=z9hG4bK1\r\n\
             From: <sip:1001@bridge>;tag=t1\r\n\
             To: <sip:1001@bridge>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 REGISTER\r\n\
             {headers}Content-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(&raw).unwrap().unwrap().0
    }

    #[test]
    fn the_expires_header_wins_over_a_contact_parameter() {
        let request = request_with("Expires: 120\r\n");
        assert_eq!(
            requested_expires(&request, "<sip:a@b>;expires=30"),
            Some(120)
        );
    }

    #[test]
    fn a_contact_expires_parameter_is_used_when_there_is_no_header() {
        let request = request_with("");
        assert_eq!(
            requested_expires(&request, "<sip:a@b>;expires=30"),
            Some(30)
        );
    }

    /// No expiry stated at all means "you choose", not "zero" — reading it as
    /// zero would de-register every phone that omits the header.
    #[test]
    fn an_absent_expiry_is_none_rather_than_zero() {
        let request = request_with("");
        assert_eq!(requested_expires(&request, "<sip:a@b>"), None);
    }

    #[test]
    fn a_zero_expiry_is_recognised_in_either_position() {
        assert_eq!(
            requested_expires(&request_with("Expires: 0\r\n"), "<sip:a@b>"),
            Some(0)
        );
        assert_eq!(
            requested_expires(&request_with(""), "<sip:a@b>;expires=0"),
            Some(0)
        );
    }
}
