//! Real Gm IPsec SA setup (TS 33.203 Annex H) — installs kernel XFRM states
//! and policies so the authenticated REGISTER can go out over an actual
//! IPsec-protected connection to the network's negotiated port, matching
//! what a real UE (and sysmocom's `volte.c`) does after a `Security-Server`
//! response comes back. See `docs/gm-ipsec-xfrm-plan.md` for the derivation
//! of this topology from a captured working registration.
//!
//! Shells out to `ip xfrm` rather than speaking raw netlink, to stay
//! consistent with this crate's zero-`unsafe` policy and the pattern
//! `supervise::epdg_iface` already uses for netns/route setup.

use crate::error::{BridgeError, BridgeResult};
use crate::ims::SaProposal;
use std::net::{IpAddr, SocketAddr};
use std::process::Command;

/// The network's counter-proposal, parsed from a `Security-Server` header
/// value (e.g. `ipsec-3gpp; q=0.1; alg=hmac-md5-96; ealg=null; spi-c=...;
/// spi-s=...; port-c=...; port-s=...`).
#[derive(Debug, Clone)]
pub struct SecurityServerParams {
    pub alg: String,
    pub ealg: String,
    pub spi_c: u32,
    pub spi_s: u32,
    pub port_c: u16,
    pub port_s: u16,
    /// The offer's `q=` preference, 0.0 when absent. Only meaningful for
    /// ranking one offer against another — see `select_security_server`.
    pub q: f32,
}

pub fn parse_security_server(header: &str) -> BridgeResult<SecurityServerParams> {
    let mut alg = None;
    let mut ealg = None;
    let mut spi_c = None;
    let mut spi_s = None;
    let mut port_c = None;
    let mut port_s = None;
    let mut q = 0.0f32;

    for field in header.split(';').skip(1) {
        let field = field.trim();
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "alg" => alg = Some(value.to_string()),
            "ealg" => ealg = Some(value.to_string()),
            "spi-c" => spi_c = value.parse::<u32>().ok(),
            "spi-s" => spi_s = value.parse::<u32>().ok(),
            "port-c" => port_c = value.parse::<u16>().ok(),
            "port-s" => port_s = value.parse::<u16>().ok(),
            // `str::parse::<f32>` accepts the literal "nan" and returns
            // `Ok(NaN)`, so `unwrap_or` alone never catches it. A `q=nan`
            // offer would then win `select_security_server`'s `best.is_none()`
            // branch unconditionally and stay unbeatable forever after --
            // `x > NaN` is false for every later `x` under IEEE 754 -- so
            // reject non-finite values explicitly rather than trusting them
            // to fail to parse.
            "q" => {
                q = value
                    .parse::<f32>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0)
            }
            _ => {}
        }
    }

    Ok(SecurityServerParams {
        alg: alg.ok_or_else(|| BridgeError::Ims("Security-Server missing alg=".into()))?,
        ealg: ealg.ok_or_else(|| BridgeError::Ims("Security-Server missing ealg=".into()))?,
        spi_c: spi_c.ok_or_else(|| BridgeError::Ims("Security-Server missing spi-c=".into()))?,
        spi_s: spi_s.ok_or_else(|| BridgeError::Ims("Security-Server missing spi-s=".into()))?,
        port_c: port_c.ok_or_else(|| BridgeError::Ims("Security-Server missing port-c=".into()))?,
        port_s: port_s.ok_or_else(|| BridgeError::Ims("Security-Server missing port-s=".into()))?,
        q,
    })
}

/// Auth/cipher algorithms `derive_auth_key`/`derive_cipher_key` can actually
/// key. `des-ede3-cbc` is deliberately absent: its 192-bit key needs the
/// TS 33.203 Annex I CK-expansion we don't implement.
const SUPPORTED_AUTH: [&str; 2] = ["hmac-sha-1-96", "hmac-md5-96"];
const SUPPORTED_CIPHER: [&str; 2] = ["aes-cbc", "null"];

/// Pick which of the network's `Security-Server` offers to build SAs from.
///
/// A P-CSCF sends one header per algorithm combination it accepts, and we have
/// to commit to exactly one *before* we can talk to it — the choice keys the
/// SAs, and the network only learns it from the `Security-Verify` we send back
/// over those very SAs. Choose the combination the P-CSCF isn't actually using
/// and the failure is mute: our ESP goes out fine, its replies arrive and fail
/// the integrity check, and the protected connection just times out
/// (`XfrmInStateProtoError` climbing in `/proc/net/xfrm_stat` is the only
/// direct evidence, observed live on Jio 2026-08-14).
///
/// Offers naming algorithms we cannot key are skipped rather than failed on —
/// picking one would abort a registration the remaining offers could complete.
/// Among what's left the highest `q` wins, which is the preference the network
/// itself expressed; `want_auth`/`want_cipher` override that when a deployment
/// has to pin a combination the q-ordering would not have chosen.
/// Returns the parsed offer alongside its verbatim header text, which RFC 3329
/// §2.4 requires be echoed back in `Security-Verify` — reserialising the parsed
/// form instead would risk differing by a space and failing that check.
pub fn select_security_server(
    offers: &[&str],
    want_auth: Option<&str>,
    want_cipher: Option<&str>,
) -> BridgeResult<(SecurityServerParams, String)> {
    let mut best: Option<(SecurityServerParams, String)> = None;
    for offer in offers {
        let Ok(params) = parse_security_server(offer) else {
            continue;
        };
        if !SUPPORTED_AUTH.contains(&params.alg.as_str())
            || !SUPPORTED_CIPHER.contains(&params.ealg.as_str())
        {
            continue;
        }
        if want_auth.is_some_and(|w| w != params.alg) {
            continue;
        }
        if want_cipher.is_some_and(|w| w != params.ealg) {
            continue;
        }
        if best.as_ref().is_none_or(|(b, _)| params.q > b.q) {
            best = Some((params, (*offer).to_string()));
        }
    }
    best.ok_or_else(|| {
        BridgeError::Ims(format!(
            "no usable Security-Server offer among {} (wanted auth={:?} cipher={:?}); \
             offered: {}",
            offers.len(),
            want_auth.unwrap_or("any"),
            want_cipher.unwrap_or("any"),
            offers.join(" | ")
        ))
    })
}

/// Kernel crypto auth algorithm name for a negotiated SIP `alg=` value.
/// Matches sysmocom's `volte.c` (`g_ipsec_alg[].kernel_name`) — the
/// non-truncated legacy `XFRMA_ALG_AUTH` names, not `auth-trunc`.
fn kernel_auth_name(alg: &str) -> BridgeResult<&'static str> {
    match alg {
        "hmac-md5-96" => Ok("md5"),
        "hmac-sha-1-96" => Ok("sha1"),
        other => Err(BridgeError::Ims(format!("unsupported auth alg: {other}"))),
    }
}

/// Kernel crypto cipher algorithm name for a negotiated SIP `ealg=` value.
fn kernel_cipher_name(ealg: &str) -> BridgeResult<&'static str> {
    match ealg {
        "aes-cbc" => Ok("cbc(aes)"),
        "null" => Ok("cipher_null"),
        other => Err(BridgeError::Ims(format!("unsupported enc alg: {other}"))),
    }
}

/// TS 33.203 Annex H: the auth/cipher keys are the AKA `IK`/`CK` used
/// directly, no KDF. `hmac-sha-1-96` needs a 160-bit key but `IK` is only
/// 128 bits, so it's zero-padded to 20 bytes (matches `volte_set_xfrm`).
fn derive_auth_key(alg: &str, ik: &[u8]) -> BridgeResult<Vec<u8>> {
    match alg {
        "hmac-md5-96" => Ok(ik.to_vec()),
        "hmac-sha-1-96" => {
            let mut key = ik.to_vec();
            key.extend_from_slice(&[0u8; 4]);
            Ok(key)
        }
        other => Err(BridgeError::Ims(format!("unsupported auth alg: {other}"))),
    }
}

fn derive_cipher_key(ealg: &str, ck: &[u8]) -> BridgeResult<Vec<u8>> {
    match ealg {
        "aes-cbc" => Ok(ck.to_vec()),
        "null" => Ok(Vec::new()),
        other => Err(BridgeError::Ims(format!("unsupported enc alg: {other}"))),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run_ip_xfrm(args: &[String]) -> BridgeResult<()> {
    tracing::debug!(args = ?args, "ip xfrm");
    let output = Command::new("ip")
        .arg("xfrm")
        .args(args)
        .output()
        .map_err(|e| BridgeError::Ims(format!("failed to spawn `ip xfrm`: {e}")))?;
    if !output.status.success() {
        return Err(BridgeError::Ims(format!(
            "ip xfrm {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn xfrm_state_add(
    src: IpAddr,
    dst: IpAddr,
    spi: u32,
    auth_name: &str,
    auth_key: &str,
    enc_name: &str,
    enc_key: &str,
) -> BridgeResult<()> {
    let mut args = vec![
        "state".to_string(),
        "add".to_string(),
        "src".to_string(),
        src.to_string(),
        "dst".to_string(),
        dst.to_string(),
        "proto".to_string(),
        "esp".to_string(),
        "spi".to_string(),
        format!("0x{spi:08x}"),
        "mode".to_string(),
        "transport".to_string(),
        "auth".to_string(),
        auth_name.to_string(),
        format!("0x{auth_key}"),
    ];
    // `ip xfrm state add ... proto esp` requires an `enc`/`aead` clause even
    // for a null cipher — but the keymat must be a truly empty string, not
    // `0x` or a dummy zero byte (either gets rejected as EINVAL, since
    // `cipher_null` expects exactly a zero-length key).
    args.push("enc".to_string());
    args.push(enc_name.to_string());
    args.push(if enc_key.is_empty() {
        String::new()
    } else {
        format!("0x{enc_key}")
    });
    run_ip_xfrm(&args)
}

fn xfrm_state_del(src: IpAddr, dst: IpAddr, spi: u32) -> BridgeResult<()> {
    run_ip_xfrm(&[
        "state".to_string(),
        "delete".to_string(),
        "src".to_string(),
        src.to_string(),
        "dst".to_string(),
        dst.to_string(),
        "proto".to_string(),
        "esp".to_string(),
        "spi".to_string(),
        format!("0x{spi:08x}"),
    ])
}

/// `ip xfrm`'s selector grammar (`UPSPEC := proto { tcp | udp | ... }
/// [sport PORT] [dport PORT]`) rejects the literal names "tcp"/"udp" on this
/// iproute2 build ("PROTO value is invalid") but accepts the numeric IP
/// protocol number.
fn ip_proto_number(proto: &str) -> BridgeResult<&'static str> {
    match proto {
        "tcp" => Ok("6"),
        "udp" => Ok("17"),
        other => Err(BridgeError::Ims(format!("unsupported proto: {other}"))),
    }
}

/// `add` fails when the selector is already in the table; `update` replaces
/// the template of one that is. `tmpl_spi` of `None` leaves the SPI out, so the
/// template matches whichever association the packet arrived on.
fn xfrm_policy_write(
    verb: &str,
    src: SocketAddr,
    dst: SocketAddr,
    tmpl_spi: Option<u32>,
    dir_in: bool,
    proto: &str,
) -> BridgeResult<()> {
    let dir = if dir_in { "in" } else { "out" };
    let proto_num = ip_proto_number(proto)?;
    let mut args = vec![
        "policy".to_string(),
        verb.to_string(),
        "src".to_string(),
        src.ip().to_string(),
        "dst".to_string(),
        dst.ip().to_string(),
        "proto".to_string(),
        proto_num.to_string(),
        "sport".to_string(),
        src.port().to_string(),
        "dport".to_string(),
        dst.port().to_string(),
        "dir".to_string(),
        dir.to_string(),
        "tmpl".to_string(),
        "src".to_string(),
        src.ip().to_string(),
        "dst".to_string(),
        dst.ip().to_string(),
        "proto".to_string(),
        "esp".to_string(),
    ];
    if let Some(spi) = tmpl_spi {
        args.push("spi".to_string());
        args.push(format!("0x{spi:08x}"));
    }
    args.push("mode".to_string());
    args.push("transport".to_string());
    run_ip_xfrm(&args)
}

fn xfrm_policy_del(
    src: SocketAddr,
    dst: SocketAddr,
    proto: &str,
    dir_in: bool,
) -> BridgeResult<()> {
    let dir = if dir_in { "in" } else { "out" };
    let proto_num = ip_proto_number(proto)?;
    run_ip_xfrm(&[
        "policy".to_string(),
        "delete".to_string(),
        "src".to_string(),
        src.ip().to_string(),
        "dst".to_string(),
        dst.ip().to_string(),
        "proto".to_string(),
        proto_num.to_string(),
        "sport".to_string(),
        src.port().to_string(),
        "dport".to_string(),
        dst.port().to_string(),
        "dir".to_string(),
        dir.to_string(),
    ])
}

/// The four endpoints of the two logical Gm tunnels (TS 33.203 Annex H):
/// "c" = our/their client-role port, "s" = our/their server-role port.
#[derive(Clone)]
pub struct GmEndpoints {
    pub local_c: SocketAddr,
    pub local_s: SocketAddr,
    pub remote_c: SocketAddr,
    pub remote_s: SocketAddr,
}

impl GmEndpoints {
    pub fn new(
        local_ip: IpAddr,
        remote_ip: IpAddr,
        ours: &SaProposal,
        theirs: &SecurityServerParams,
    ) -> Self {
        Self {
            local_c: SocketAddr::new(local_ip, ours.port_c),
            local_s: SocketAddr::new(local_ip, ours.port_s),
            remote_c: SocketAddr::new(remote_ip, theirs.port_c),
            remote_s: SocketAddr::new(remote_ip, theirs.port_s),
        }
    }
}

/// Install the 4 XFRM states + 4 XFRM policies for the two Gm tunnels —
/// mirrors `volte_set_xfrm()`/`volte_alloc_spi()` exactly (see
/// `docs/gm-ipsec-xfrm-plan.md` for the derivation):
///
/// - Tunnel A (client-initiated: our `local_c` <-> their `remote_s`) is what
///   carries our authenticated REGISTER and its response.
/// - Tunnel B (server-initiated: our `local_s` <-> their `remote_c`) carries
///   everything the *network* originates — the reg-event `NOTIFY` and every
///   mobile-terminating `INVITE`. `sip_client::spawn_gm_server` listens on
///   `local_s` for exactly this; without a listener there the kernel RSTs the
///   P-CSCF's connection attempt and inbound calls are never delivered at all,
///   while REGISTER and outbound calls (both client-initiated, tunnel A) keep
///   working and hide the fault.
pub fn install_gm_sas(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    proto: &str,
    ik: &[u8],
    ck: &[u8],
) -> BridgeResult<()> {
    let keys = SaKeys::derive(theirs, ik, ck)?;
    let set = SaSet {
        endpoints: endpoints.clone(),
        ours: ours.clone(),
        theirs: theirs.clone(),
    };
    // TS 33.203: one set of Gm SAs protects **both** transports between the
    // negotiated port pairs. Which one the network uses for a request it
    // originates is its choice, not ours, so `proto` (the transport we picked
    // for our own client connection) must not decide what we accept.
    //
    // Installing only our own transport made inbound calls impossible.
    // Measured on Jio 2026-08-14: it delivers network-initiated INVITEs over
    // UDP, so with TCP-only policies the ESP arrived, decrypted, and landed on
    // a port with no UDP socket — the kernel answered ICMP port-unreachable
    // and the caller heard "out of coverage area". Nothing in
    // /proc/net/xfrm_stat records that, and the SAs themselves are
    // wildcard-selector so they were never the limiting factor; only these
    // selectors were.
    let _ = proto;
    for state in set.states() {
        IpXfrm.state_add(&state, &keys)?;
    }
    for policy in set.policies() {
        IpXfrm.policy_add(&policy)?;
    }
    Ok(())
}

/// Best-effort cleanup — logs failures rather than propagating them, since
/// this typically runs on an already-failing path and shouldn't mask the
/// original error.
pub fn remove_gm_sas(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    proto: &str,
) {
    // `install_gm_sas` installs 8 policies (4 canonical + 4 cross-product) per
    // protocol, for both "tcp" and "udp" — 16 in total, regardless of which
    // transport `proto` names. This must delete exactly that set, or every
    // re-registration leaks 12 of the 16 (found in review: this used to
    // delete only the 4 canonical policies for the single passed-in `proto`).
    // A stale policy left in the kernel table can match live traffic on a
    // reused port pairing after the SA generation it referenced is gone,
    // silently dropping packets in a way indistinguishable from the Jio
    // inbound-routing bug the cross-product policies exist to fix.
    let _ = proto;
    let set = SaSet {
        endpoints: endpoints.clone(),
        ours: ours.clone(),
        theirs: theirs.clone(),
    };
    set.remove(&IpXfrm);
}

// --- the sets, and replacing one with another -------------------------------

/// The key material of one Gm association, in the names the kernel wants.
pub(crate) struct SaKeys {
    auth_name: &'static str,
    auth_key: String,
    enc_name: &'static str,
    enc_key: String,
}

impl SaKeys {
    /// TS 33.203 keys all four SAs identically from one `IK` (and `CK`).
    pub(crate) fn derive(
        theirs: &SecurityServerParams,
        ik: &[u8],
        ck: &[u8],
    ) -> BridgeResult<Self> {
        Ok(Self {
            auth_name: kernel_auth_name(&theirs.alg)?,
            auth_key: hex_encode(&derive_auth_key(&theirs.alg, ik)?),
            enc_name: kernel_cipher_name(&theirs.ealg)?,
            enc_key: hex_encode(&derive_cipher_key(&theirs.ealg, ck)?),
        })
    }
}

/// One ESP state: `src` to `dst`, identified by `spi`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StateSpec {
    pub(crate) src: IpAddr,
    pub(crate) dst: IpAddr,
    pub(crate) spi: u32,
}

/// One policy. Its selector (everything but the template) is what the kernel
/// keys it on; `spi` is the template's SPI, `None` for any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PolicySpec {
    pub(crate) src: SocketAddr,
    pub(crate) dst: SocketAddr,
    pub(crate) proto: &'static str,
    pub(crate) dir_in: bool,
    pub(crate) spi: Option<u32>,
}

impl PolicySpec {
    fn same_selector(&self, other: &PolicySpec) -> bool {
        (self.src, self.dst, self.proto, self.dir_in)
            == (other.src, other.dst, other.proto, other.dir_in)
    }
}

/// What the kernel is asked to do. The real one shells out to `ip xfrm`; a
/// test substitutes a table that behaves like the kernel's (a duplicate `add`
/// fails, `update` of a missing selector fails), because the staging logic is
/// all about those rules and CI cannot create XFRM state without privileges.
pub(crate) trait XfrmBackend: Send {
    fn state_add(&self, state: &StateSpec, keys: &SaKeys) -> BridgeResult<()>;
    fn state_del(&self, state: &StateSpec) -> BridgeResult<()>;
    fn policy_add(&self, policy: &PolicySpec) -> BridgeResult<()>;
    fn policy_update(&self, policy: &PolicySpec) -> BridgeResult<()>;
    fn policy_del(&self, policy: &PolicySpec) -> BridgeResult<()>;
}

/// The kernel, through `ip xfrm`.
pub(crate) struct IpXfrm;

impl XfrmBackend for IpXfrm {
    fn state_add(&self, s: &StateSpec, keys: &SaKeys) -> BridgeResult<()> {
        xfrm_state_add(
            s.src,
            s.dst,
            s.spi,
            keys.auth_name,
            &keys.auth_key,
            keys.enc_name,
            &keys.enc_key,
        )
    }
    fn state_del(&self, s: &StateSpec) -> BridgeResult<()> {
        xfrm_state_del(s.src, s.dst, s.spi)
    }
    fn policy_add(&self, p: &PolicySpec) -> BridgeResult<()> {
        xfrm_policy_write("add", p.src, p.dst, p.spi, p.dir_in, p.proto)
    }
    fn policy_update(&self, p: &PolicySpec) -> BridgeResult<()> {
        xfrm_policy_write("update", p.src, p.dst, p.spi, p.dir_in, p.proto)
    }
    fn policy_del(&self, p: &PolicySpec) -> BridgeResult<()> {
        xfrm_policy_del(p.src, p.dst, p.proto, p.dir_in)
    }
}

/// The endpoints and identifiers of one generation of Gm associations.
#[derive(Clone)]
pub(crate) struct SaSet {
    pub(crate) endpoints: GmEndpoints,
    pub(crate) ours: SaProposal,
    pub(crate) theirs: SecurityServerParams,
}

impl SaSet {
    /// The four ESP states, in the order they are installed.
    pub(crate) fn states(&self) -> Vec<StateSpec> {
        let (e, o, t) = (&self.endpoints, &self.ours, &self.theirs);
        vec![
            // Outbound: we send, tagged with the SPI *they* told us to use.
            StateSpec {
                src: e.local_c.ip(),
                dst: e.remote_s.ip(),
                spi: t.spi_s,
            },
            StateSpec {
                src: e.local_s.ip(),
                dst: e.remote_c.ip(),
                spi: t.spi_c,
            },
            // Inbound: they send, tagged with the SPI *we* told them to use.
            StateSpec {
                src: e.remote_c.ip(),
                dst: e.local_s.ip(),
                spi: o.spi_s,
            },
            StateSpec {
                src: e.remote_s.ip(),
                dst: e.local_c.ip(),
                spi: o.spi_c,
            },
        ]
    }

    /// The sixteen policies (eight per protocol), in the order they are
    /// installed.
    ///
    /// The four canonical pairs are what TS 33.203 draws, but they only cover a
    /// client port talking to the *opposite* server port. RFC 3261 §18.2.2
    /// routes a response to the `Via` sent-by, and Jio's P-CSCF sends
    /// network-initiated requests from its client port while naming its
    /// *server* port in that `Via` — so the reply belongs on a pairing the four
    /// canonical policies do not describe, and would otherwise leave
    /// unprotected (silently: an unmatched policy is not an error, the packet
    /// just goes out in the clear and the carrier drops it).
    ///
    /// Completing the cross product costs four more selectors per protocol and
    /// makes every combination of our two protected ports with their two
    /// protected ports carry ESP, so response routing is free to follow the SIP
    /// rules without a transport-layer trap underneath it.
    pub(crate) fn policies(&self) -> Vec<PolicySpec> {
        let (e, o, t) = (&self.endpoints, &self.ours, &self.theirs);
        let mut out = Vec::with_capacity(16);
        for proto in ["tcp", "udp"] {
            let policy = |src, dst, spi, dir_in| PolicySpec {
                src,
                dst,
                proto,
                dir_in,
                spi: Some(spi),
            };
            out.extend([
                policy(e.local_c, e.remote_s, t.spi_s, false),
                policy(e.local_s, e.remote_c, t.spi_c, false),
                policy(e.remote_c, e.local_s, o.spi_s, true),
                policy(e.remote_s, e.local_c, o.spi_c, true),
                policy(e.local_c, e.remote_c, t.spi_c, false),
                policy(e.local_s, e.remote_s, t.spi_s, false),
                policy(e.remote_c, e.local_c, o.spi_c, true),
                policy(e.remote_s, e.local_s, o.spi_s, true),
            ]);
        }
        out
    }

    /// Best-effort removal of everything this set installed. Failures are
    /// logged, not returned: this runs on paths that are already failing, or
    /// are cleaning up after a drain, and must not mask what came before.
    pub(crate) fn remove(&self, backend: &dyn XfrmBackend) {
        for state in self.states() {
            if let Err(e) = backend.state_del(&state) {
                tracing::warn!(what = "state", error = %e, "failed to clean up Gm IPsec state");
            }
        }
        for policy in self.policies() {
            if let Err(e) = backend.policy_del(&policy) {
                tracing::warn!(what = "policy", error = %e, "failed to clean up Gm IPsec state");
            }
        }
    }
}

/// A replacement generation of SAs installed beside the working one (TS 33.203
/// §7.4), not yet carrying anything.
///
/// - States are new and distinct (new SPIs), so they sit beside the old ones.
/// - A policy whose selector the old generation does not have is simply added.
/// - A selector both generations use (our protected server port is unchanged,
///   and the network may keep its own ports) cannot hold two policies. Its
///   *inbound* template is widened to any SPI at once, so a packet from either
///   generation is accepted; its *outbound* template keeps the old SPI, so
///   nothing we send changes until [`StagedSas::activate`].
///
/// Dropping it without activating removes everything it added and restores
/// what it widened, leaving the working generation exactly as it was.
pub(crate) struct StagedSas {
    backend: Option<Box<dyn XfrmBackend>>,
    old: SaSet,
    new: SaSet,
    states: Vec<StateSpec>,
    added: Vec<PolicySpec>,
    /// Inbound policies widened to any SPI, as they were before.
    widened: Vec<PolicySpec>,
    /// Outbound policies to repoint at activation, with the template they had.
    repoint: Vec<(PolicySpec, PolicySpec)>,
}

impl StagedSas {
    /// Installs `new` beside `old`. On any failure what was done is undone and
    /// the working generation is untouched.
    pub(crate) fn stage(
        backend: Box<dyn XfrmBackend>,
        old: &SaSet,
        new: SaSet,
        keys: &SaKeys,
    ) -> BridgeResult<Self> {
        let old_states = old.states();
        if let Some(clash) = new.states().into_iter().find(|s| old_states.contains(s)) {
            return Err(BridgeError::Ims(format!(
                "the replacement Gm SA reuses the working one's SPI {:#010x}",
                clash.spi
            )));
        }
        let mut staged = Self {
            backend: Some(backend),
            old: old.clone(),
            new,
            states: Vec::new(),
            added: Vec::new(),
            widened: Vec::new(),
            repoint: Vec::new(),
        };
        // A failure part-way returns early; `staged` is dropped and rolls back.
        staged.install(keys)?;
        Ok(staged)
    }

    fn backend(&self) -> &dyn XfrmBackend {
        self.backend
            .as_deref()
            .expect("the backend is only taken when the set is finished")
    }

    fn install(&mut self, keys: &SaKeys) -> BridgeResult<()> {
        for state in self.new.states() {
            self.backend().state_add(&state, keys)?;
            self.states.push(state);
        }
        let old_policies = self.old.policies();
        for policy in self.new.policies() {
            match old_policies.iter().find(|o| o.same_selector(&policy)) {
                None => {
                    self.backend().policy_add(&policy)?;
                    self.added.push(policy);
                }
                Some(current) if policy.dir_in => {
                    let any = PolicySpec {
                        spi: None,
                        ..policy
                    };
                    self.backend().policy_update(&any)?;
                    self.widened.push(current.clone());
                }
                Some(current) => self.repoint.push((policy, current.clone())),
            }
        }
        Ok(())
    }

    /// The replacement is confirmed: outbound traffic on shared selectors moves
    /// to it, and the working generation is handed back to be removed once the
    /// drain is over. If a policy cannot be moved, nothing is activated.
    pub(crate) fn activate(mut self) -> BridgeResult<RetiredSas> {
        for (moved, (new, _)) in self.repoint.iter().enumerate() {
            if let Err(e) = self.backend().policy_update(new) {
                for (_, was) in &self.repoint[..moved] {
                    let _ = self.backend().policy_update(was);
                }
                return Err(e);
            }
        }
        let kept = self.new.policies();
        let backend = self.backend.take().expect("not yet finished");
        // What the new generation shares with the old stays; the rest of the
        // old generation goes at the end of the drain.
        let stale: Vec<PolicySpec> = self
            .old
            .policies()
            .into_iter()
            .filter(|o| !kept.iter().any(|k| k.same_selector(o)))
            .collect();
        Ok(RetiredSas {
            backend: Some(backend),
            states: self.old.states(),
            policies: stale,
        })
    }

    fn roll_back(&mut self) {
        let Some(backend) = self.backend.take() else {
            return;
        };
        let warn = |what: &str, result: BridgeResult<()>| {
            if let Err(e) = result {
                tracing::warn!(what, error = %e, "failed to roll back a staged Gm IPsec SA");
            }
        };
        for policy in self.added.drain(..) {
            warn("policy", backend.policy_del(&policy));
        }
        for was in self.widened.drain(..) {
            warn("policy", backend.policy_update(&was));
        }
        for state in self.states.drain(..) {
            warn("state", backend.state_del(&state));
        }
    }
}

impl Drop for StagedSas {
    fn drop(&mut self) {
        self.roll_back();
    }
}

/// The generation of SAs that a replacement took over from. Its states and the
/// policies only it used stay in place for pending transactions and are removed
/// by [`RetiredSas::remove`] or, failing that, when it is dropped.
pub(crate) struct RetiredSas {
    backend: Option<Box<dyn XfrmBackend>>,
    states: Vec<StateSpec>,
    policies: Vec<PolicySpec>,
}

impl RetiredSas {
    pub(crate) fn remove(&mut self) {
        let Some(backend) = self.backend.take() else {
            return;
        };
        for policy in &self.policies {
            if let Err(e) = backend.policy_del(policy) {
                tracing::warn!(what = "policy", error = %e, "failed to remove a retired Gm IPsec SA");
            }
        }
        for state in &self.states {
            if let Err(e) = backend.state_del(state) {
                tracing::warn!(what = "state", error = %e, "failed to remove a retired Gm IPsec SA");
            }
        }
    }
}

impl Drop for RetiredSas {
    fn drop(&mut self) {
        self.remove();
    }
}

/// A kernel table for tests: the rules the staging logic lives by (a selector
/// holds one policy, `delete` of nothing fails). CI cannot create XFRM state
/// without privileges, so this stands in for `ip xfrm`; the real command was
/// checked against a kernel in the privileged container.
#[cfg(test)]
pub(crate) mod fake_kernel {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A kernel table: the rules the staging logic lives by (a selector holds
    /// one policy, `update` needs one to exist, `delete` of nothing fails).
    #[derive(Default)]
    pub(crate) struct Table {
        pub(crate) states: Vec<StateSpec>,
        pub(crate) policies: Vec<PolicySpec>,
        /// Fail the nth mutating call (counted from 0), to cut a stage short.
        pub(crate) fail_at: Option<usize>,
        pub(crate) calls: usize,
    }

    #[derive(Clone, Default)]
    pub(crate) struct Kernel(pub(crate) Arc<Mutex<Table>>);

    impl Kernel {
        fn touch(&self) -> BridgeResult<()> {
            let mut t = self.0.lock().unwrap();
            t.calls += 1;
            if t.fail_at == Some(t.calls - 1) {
                return Err(BridgeError::Ims("injected kernel failure".into()));
            }
            Ok(())
        }
        pub(crate) fn policies(&self) -> Vec<PolicySpec> {
            self.0.lock().unwrap().policies.clone()
        }
        pub(crate) fn states(&self) -> Vec<StateSpec> {
            self.0.lock().unwrap().states.clone()
        }
    }

    impl XfrmBackend for Kernel {
        fn state_add(&self, s: &StateSpec, _: &SaKeys) -> BridgeResult<()> {
            self.touch()?;
            let mut t = self.0.lock().unwrap();
            if t.states.contains(s) {
                return Err(BridgeError::Ims("EEXIST".into()));
            }
            t.states.push(s.clone());
            Ok(())
        }
        fn state_del(&self, s: &StateSpec) -> BridgeResult<()> {
            self.touch()?;
            let mut t = self.0.lock().unwrap();
            let before = t.states.len();
            t.states.retain(|x| x != s);
            (t.states.len() < before)
                .then_some(())
                .ok_or_else(|| BridgeError::Ims("ENOENT".into()))
        }
        fn policy_add(&self, p: &PolicySpec) -> BridgeResult<()> {
            self.touch()?;
            let mut t = self.0.lock().unwrap();
            if t.policies.iter().any(|x| x.same_selector(p)) {
                return Err(BridgeError::Ims("EEXIST".into()));
            }
            t.policies.push(p.clone());
            Ok(())
        }
        fn policy_update(&self, p: &PolicySpec) -> BridgeResult<()> {
            self.touch()?;
            let mut t = self.0.lock().unwrap();
            let slot = t
                .policies
                .iter_mut()
                .find(|x| x.same_selector(p))
                .ok_or_else(|| BridgeError::Ims("ENOENT".into()))?;
            *slot = p.clone();
            Ok(())
        }
        fn policy_del(&self, p: &PolicySpec) -> BridgeResult<()> {
            self.touch()?;
            let mut t = self.0.lock().unwrap();
            let before = t.policies.len();
            t.policies.retain(|x| !x.same_selector(p));
            (t.policies.len() < before)
                .then_some(())
                .ok_or_else(|| BridgeError::Ims("ENOENT".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_security_server_extracts_all_fields() {
        let header = "ipsec-3gpp; q=0.1; alg=hmac-md5-96; ealg=null; spi-c=5764929; spi-s=5764928; port-c=32805; port-s=6000";
        let params = parse_security_server(header).unwrap();
        assert_eq!(params.alg, "hmac-md5-96");
        assert_eq!(params.ealg, "null");
        assert_eq!(params.spi_c, 5764929);
        assert_eq!(params.spi_s, 5764928);
        assert_eq!(params.port_c, 32805);
        assert_eq!(params.port_s, 6000);
    }

    #[test]
    fn parse_security_server_rejects_missing_field() {
        let header = "ipsec-3gpp; alg=hmac-md5-96; ealg=null; spi-c=1; spi-s=2; port-c=3";
        assert!(parse_security_server(header).is_err());
    }

    /// Jio's real offer list, verbatim from a live `401` (2026-08-14).
    const JIO_OFFERS: [&str; 6] = [
        "ipsec-3gpp;q=0.6;alg=hmac-md5-96;ealg=aes-cbc;spi-c=1;spi-s=2;port-c=32920;port-s=5067",
        "ipsec-3gpp;q=0.4;alg=hmac-md5-96;ealg=des-ede3-cbc;spi-c=1;spi-s=2;port-c=32920;port-s=5067",
        "ipsec-3gpp;q=0.3;alg=hmac-md5-96;ealg=null;spi-c=1;spi-s=2;port-c=32920;port-s=5067",
        "ipsec-3gpp;q=0.1;alg=hmac-sha-1-96;ealg=des-ede3-cbc;spi-c=1;spi-s=2;port-c=32920;port-s=5067",
        "ipsec-3gpp;q=0.2;alg=hmac-sha-1-96;ealg=aes-cbc;spi-c=1;spi-s=2;port-c=32920;port-s=5067",
        "ipsec-3gpp;q=0.5;alg=hmac-sha-1-96;ealg=null;spi-c=1;spi-s=2;port-c=32920;port-s=5067",
    ];

    #[test]
    fn select_takes_the_highest_q_supported_offer() {
        let (params, _) = select_security_server(&JIO_OFFERS, None, None).unwrap();
        assert_eq!(params.alg, "hmac-md5-96");
        assert_eq!(params.ealg, "aes-cbc");
        assert_eq!(params.q, 0.6);
    }

    #[test]
    fn select_can_pin_an_algorithm_the_q_order_would_not_have_chosen() {
        // The whole point of the override: q=0.2 loses on preference, but is
        // what we need when the P-CSCF advertises one thing and uses another.
        let (params, raw) =
            select_security_server(&JIO_OFFERS, Some("hmac-sha-1-96"), Some("aes-cbc")).unwrap();
        assert_eq!(params.alg, "hmac-sha-1-96");
        assert_eq!(params.ealg, "aes-cbc");
        assert_eq!(params.q, 0.2);
        assert_eq!(raw, JIO_OFFERS[4], "Security-Verify must echo it verbatim");
    }

    #[test]
    fn select_skips_offers_whose_algorithms_cannot_be_keyed() {
        // des-ede3-cbc needs the Annex I CK-expansion we don't implement;
        // choosing it would abort a registration the others could complete.
        let only_3des = [JIO_OFFERS[1], JIO_OFFERS[3]];
        assert!(select_security_server(&only_3des, None, None).is_err());
    }

    #[test]
    fn select_errors_when_the_pinned_combination_is_not_offered() {
        let err = select_security_server(&JIO_OFFERS, Some("hmac-md5-96"), Some("bogus-cbc"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no usable Security-Server offer"), "{err}");
    }

    #[test]
    fn select_treats_a_missing_q_as_lowest_preference() {
        let no_q = "ipsec-3gpp;alg=hmac-sha-1-96;ealg=aes-cbc;spi-c=1;spi-s=2;port-c=1;port-s=2";
        let (params, _) = select_security_server(&[no_q, JIO_OFFERS[0]], None, None).unwrap();
        assert_eq!(params.alg, "hmac-md5-96", "the q=0.6 offer must still win");
    }

    /// `f32::parse` accepts the literal `"nan"` — a `q=nan` offer must not
    /// win `best.is_none()` unconditionally and then be unbeatable forever
    /// after (`x > NaN` is false for every later `x`).
    #[test]
    fn select_treats_a_non_finite_q_as_lowest_preference() {
        let nan_first =
            "ipsec-3gpp;q=nan;alg=hmac-sha-1-96;ealg=aes-cbc;spi-c=1;spi-s=2;port-c=1;port-s=2";
        let (params, _) = select_security_server(&[nan_first, JIO_OFFERS[0]], None, None).unwrap();
        assert_eq!(
            params.alg, "hmac-md5-96",
            "the real q=0.6 offer must win, not NaN"
        );

        let (params, _) = parse_security_server(nan_first).map(|p| (p, ())).unwrap();
        assert_eq!(
            params.q, 0.0,
            "a non-finite q parses as the lowest preference, not NaN"
        );
    }

    #[test]
    fn derive_auth_key_pads_sha1_to_20_bytes() {
        let ik = [0xAAu8; 16];
        let md5_key = derive_auth_key("hmac-md5-96", &ik).unwrap();
        assert_eq!(md5_key, ik.to_vec());
        let sha1_key = derive_auth_key("hmac-sha-1-96", &ik).unwrap();
        assert_eq!(sha1_key.len(), 20);
        assert_eq!(&sha1_key[0..16], &ik[..]);
        assert_eq!(&sha1_key[16..20], &[0u8; 4]);
    }

    #[test]
    fn derive_cipher_key_empty_for_null() {
        let ck = [0xBBu8; 16];
        assert_eq!(derive_cipher_key("null", &ck).unwrap(), Vec::<u8>::new());
        assert_eq!(derive_cipher_key("aes-cbc", &ck).unwrap(), ck.to_vec());
    }

    #[test]
    fn kernel_names_map_sip_algs_to_kernel_crypto_names() {
        assert_eq!(kernel_auth_name("hmac-md5-96").unwrap(), "md5");
        assert_eq!(kernel_auth_name("hmac-sha-1-96").unwrap(), "sha1");
        assert_eq!(kernel_cipher_name("null").unwrap(), "cipher_null");
        assert_eq!(kernel_cipher_name("aes-cbc").unwrap(), "cbc(aes)");
    }

    // --- staging ---------------------------------------------------------

    use super::fake_kernel::Kernel;

    fn keys() -> SaKeys {
        SaKeys {
            auth_name: "hmac(md5)",
            auth_key: String::new(),
            enc_name: "ecb(cipher_null)",
            enc_key: String::new(),
        }
    }

    /// A generation on one loopback pair. Our server port and the network's
    /// ports are the same in every generation, as they are on the carriers
    /// that keep them; only our client port and the SPIs differ.
    fn generation(port_c: u16, spi_base: u32) -> SaSet {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        SaSet {
            endpoints: GmEndpoints {
                local_c: SocketAddr::new(ip("10.0.0.2"), port_c),
                local_s: SocketAddr::new(ip("10.0.0.2"), 5062),
                remote_c: SocketAddr::new(ip("10.0.0.1"), 6001),
                remote_s: SocketAddr::new(ip("10.0.0.1"), 6002),
            },
            ours: SaProposal {
                spi_c: spi_base + 1,
                spi_s: spi_base + 2,
                port_c,
                port_s: 5062,
            },
            theirs: SecurityServerParams {
                alg: "hmac-md5-96".into(),
                ealg: "null".into(),
                spi_c: spi_base + 3,
                spi_s: spi_base + 4,
                port_c: 6001,
                port_s: 6002,
                q: 0.0,
            },
        }
    }

    fn working(kernel: &Kernel, set: &SaSet) {
        for s in set.states() {
            kernel.state_add(&s, &keys()).unwrap();
        }
        for p in set.policies() {
            kernel.policy_add(&p).unwrap();
        }
    }

    #[test]
    fn a_generation_is_four_states_and_sixteen_policies() {
        let set = generation(5060, 0x100);
        assert_eq!(set.states().len(), 4);
        let policies = set.policies();
        assert_eq!(policies.len(), 16);
        let kernel = Kernel::default();
        working(&kernel, &set);
        assert_eq!(kernel.policies().len(), 16, "no two share a selector");
    }

    #[test]
    fn staging_adds_beside_the_working_generation_and_changes_nothing_it_carries() {
        let (old, new) = (generation(5060, 0x100), generation(5070, 0x200));
        let kernel = Kernel::default();
        working(&kernel, &old);
        let before = kernel.policies();

        let staged =
            StagedSas::stage(Box::new(kernel.clone()), &old, new.clone(), &keys()).unwrap();

        assert_eq!(kernel.states().len(), 8, "both generations' states exist");
        // Outbound on a selector both generations use still names the old SPI,
        // so nothing we send changes before activation.
        for was in before.iter().filter(|p| !p.dir_in) {
            let now = kernel
                .policies()
                .into_iter()
                .find(|p| p.same_selector(was))
                .unwrap();
            assert_eq!(now.spi, was.spi, "{was:?}");
        }
        // Inbound on a shared selector accepts either generation.
        let shared_in = kernel
            .policies()
            .into_iter()
            .find(|p| p.dir_in && p.src == new.endpoints.remote_c && p.dst == new.endpoints.local_s)
            .unwrap();
        assert_eq!(shared_in.spi, None);
        // The new client port's selectors exist.
        assert!(kernel
            .policies()
            .iter()
            .any(|p| p.src == new.endpoints.local_c
                && p.dst == new.endpoints.remote_s
                && !p.dir_in));
        drop(staged);
    }

    #[test]
    fn dropping_a_staged_set_restores_the_working_generation_exactly() {
        let (old, new) = (generation(5060, 0x100), generation(5070, 0x200));
        let kernel = Kernel::default();
        working(&kernel, &old);
        let (states, policies) = (kernel.states(), kernel.policies());

        drop(StagedSas::stage(Box::new(kernel.clone()), &old, new, &keys()).unwrap());

        assert_eq!(kernel.states(), states);
        assert_eq!(kernel.policies(), policies);
    }

    #[test]
    fn a_failure_part_way_through_staging_leaves_nothing_behind() {
        let (old, new) = (generation(5060, 0x100), generation(5070, 0x200));
        // Cut it short at every kernel call it makes, early and late.
        for fail_at in 0..40 {
            let kernel = Kernel::default();
            working(&kernel, &old);
            let (states, policies) = (kernel.states(), kernel.policies());
            {
                let mut t = kernel.0.lock().unwrap();
                t.calls = 0;
                t.fail_at = Some(fail_at);
            }
            let result = StagedSas::stage(Box::new(kernel.clone()), &old, new.clone(), &keys());
            kernel.0.lock().unwrap().fail_at = None;
            drop(result);
            assert_eq!(kernel.states(), states, "fail_at {fail_at}");
            assert_eq!(kernel.policies(), policies, "fail_at {fail_at}");
        }
    }

    #[test]
    fn activation_moves_outbound_to_the_new_spis_and_retires_only_the_old_generation() {
        let (old, new) = (generation(5060, 0x100), generation(5070, 0x200));
        let kernel = Kernel::default();
        working(&kernel, &old);
        let staged =
            StagedSas::stage(Box::new(kernel.clone()), &old, new.clone(), &keys()).unwrap();

        let mut retired = staged.activate().unwrap();

        for want in new.policies().iter().filter(|p| !p.dir_in) {
            let now = kernel
                .policies()
                .into_iter()
                .find(|p| p.same_selector(want))
                .unwrap();
            assert_eq!(now.spi, want.spi, "{want:?}");
        }
        assert_eq!(
            kernel.states().len(),
            8,
            "the old states stay for the drain"
        );

        retired.remove();

        assert_eq!(kernel.states().len(), 4);
        // Every selector of the new generation is present; none of the old
        // client port's remain.
        for want in new.policies() {
            assert!(
                kernel.policies().iter().any(|p| p.same_selector(&want)),
                "{want:?}"
            );
        }
        assert!(kernel
            .policies()
            .iter()
            .all(|p| p.src.port() != 5060 && p.dst.port() != 5060));
        assert_eq!(kernel.policies().len(), 16);
    }

    #[test]
    fn a_retired_generation_is_removed_when_dropped_too() {
        let (old, new) = (generation(5060, 0x100), generation(5070, 0x200));
        let kernel = Kernel::default();
        working(&kernel, &old);
        let retired = StagedSas::stage(Box::new(kernel.clone()), &old, new, &keys())
            .unwrap()
            .activate()
            .unwrap();
        drop(retired);
        assert_eq!(kernel.states().len(), 4);
    }

    #[test]
    fn a_failed_activation_rolls_everything_back() {
        let (old, new) = (generation(5060, 0x100), generation(5070, 0x200));
        let kernel = Kernel::default();
        working(&kernel, &old);
        let (states, policies) = (kernel.states(), kernel.policies());
        let staged = StagedSas::stage(Box::new(kernel.clone()), &old, new, &keys()).unwrap();
        {
            let mut t = kernel.0.lock().unwrap();
            t.calls = 0;
            t.fail_at = Some(1);
        }
        assert!(staged.activate().is_err());
        kernel.0.lock().unwrap().fail_at = None;
        assert_eq!(kernel.states(), states);
        assert_eq!(kernel.policies(), policies);
    }

    #[test]
    fn a_replacement_that_reuses_a_working_spi_is_refused_before_touching_the_kernel() {
        let old = generation(5060, 0x100);
        let new = generation(5070, 0x100);
        let kernel = Kernel::default();
        working(&kernel, &old);
        let calls = kernel.0.lock().unwrap().calls;
        assert!(StagedSas::stage(Box::new(kernel.clone()), &old, new, &keys()).is_err());
        assert_eq!(kernel.0.lock().unwrap().calls, calls);
    }
}
