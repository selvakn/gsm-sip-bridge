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
    // XFRM command lines contain AKA-derived keys. Neither diagnostic logs nor
    // an error returned to the caller may include those values.
    let safe_args = redacted_xfrm_args(args);
    tracing::debug!(args = ?safe_args, "ip xfrm");
    let output = Command::new("ip")
        .arg("xfrm")
        .args(args)
        .output()
        .map_err(|e| BridgeError::Ims(format!("failed to spawn `ip xfrm`: {e}")))?;
    if !output.status.success() {
        return Err(BridgeError::Ims(format!(
            "ip xfrm {} failed: {}",
            safe_args.join(" "),
            redact_xfrm_stderr(args, &String::from_utf8_lossy(&output.stderr))
        )));
    }
    Ok(())
}

fn redacted_xfrm_args(args: &[String]) -> Vec<String> {
    let mut redacted = args.to_vec();
    for (index, arg) in args.iter().enumerate() {
        if matches!(arg.as_str(), "auth" | "auth-trunc" | "enc" | "aead") {
            if let Some(key) = redacted.get_mut(index + 2) {
                *key = "<redacted>".to_string();
            }
        }
    }
    redacted
}

fn redact_xfrm_stderr(args: &[String], stderr: &str) -> String {
    let mut redacted = stderr.trim().to_string();
    for (index, arg) in args.iter().enumerate() {
        if matches!(arg.as_str(), "auth" | "auth-trunc" | "enc" | "aead") {
            if let Some(key) = args.get(index + 2).filter(|key| !key.is_empty()) {
                redacted = redacted.replace(key, "<redacted>");
            }
        }
    }
    redacted
}

fn xfrm_state_add_args(
    src: IpAddr,
    dst: IpAddr,
    spi: u32,
    auth_name: &str,
    auth_key: &str,
    enc_name: &str,
    enc_key: &str,
) -> Vec<String> {
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
    args
}

fn xfrm_state_del_args(src: IpAddr, dst: IpAddr, spi: u32) -> Vec<String> {
    vec![
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
    ]
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct GmPolicy {
    src: SocketAddr,
    dst: SocketAddr,
    spi: u32,
    inbound: bool,
    proto: &'static str,
}

impl GmPolicy {
    fn same_selector(&self, other: &Self) -> bool {
        self.src == other.src
            && self.dst == other.dst
            && self.inbound == other.inbound
            && self.proto == other.proto
    }
}

fn xfrm_policy_args(policy: &GmPolicy, action: &str) -> Vec<String> {
    let dir = if policy.inbound { "in" } else { "out" };
    let proto_num = ip_proto_number(policy.proto).expect("internal Gm policy protocol");
    let mut args = vec![
        "policy".to_string(),
        action.to_string(),
        "src".to_string(),
        policy.src.ip().to_string(),
        "dst".to_string(),
        policy.dst.ip().to_string(),
        "proto".to_string(),
        proto_num.to_string(),
        "sport".to_string(),
        policy.src.port().to_string(),
        "dport".to_string(),
        policy.dst.port().to_string(),
        "dir".to_string(),
        dir.to_string(),
    ];
    if action == "delete" {
        return args;
    }
    args.extend([
        "tmpl".to_string(),
        "src".to_string(),
        policy.src.ip().to_string(),
        "dst".to_string(),
        policy.dst.ip().to_string(),
        "proto".to_string(),
        "esp".to_string(),
        "spi".to_string(),
        format!("0x{:08x}", policy.spi),
        "mode".to_string(),
        "transport".to_string(),
    ]);
    args
}

/// The four endpoints of the two logical Gm tunnels (TS 33.203 Annex H):
/// "c" = our/their client-role port, "s" = our/their server-role port.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// All protected port pairs, for both transports. A network request can use
/// either transport and a response can target the peer's server port named in
/// Via, so the complete cross product is needed (see the Jio capture in
/// `docs/gm-ipsec-xfrm-plan.md`).
fn gm_policies(endpoints: &GmEndpoints, theirs: &SecurityServerParams) -> Vec<GmPolicy> {
    let mut policies = Vec::with_capacity(16);
    for proto in ["tcp", "udp"] {
        for local in [endpoints.local_c, endpoints.local_s] {
            for (remote, outbound_spi) in [
                (endpoints.remote_c, theirs.spi_c),
                (endpoints.remote_s, theirs.spi_s),
            ] {
                policies.push(GmPolicy {
                    src: local,
                    dst: remote,
                    spi: outbound_spi,
                    inbound: false,
                    proto,
                });
                // An inbound policy requires ESP but deliberately leaves the
                // SPI unspecified. The ESP header still selects an exact,
                // authenticated state. This lets an unchanged server-port
                // selector accept both old and new SA generations during AKA.
                policies.push(GmPolicy {
                    src: remote,
                    dst: local,
                    spi: 0,
                    inbound: true,
                    proto,
                });
            }
        }
    }
    policies
}

fn gm_state_ids(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
) -> [(IpAddr, IpAddr, u32); 4] {
    [
        (
            endpoints.local_c.ip(),
            endpoints.remote_s.ip(),
            theirs.spi_s,
        ),
        (
            endpoints.local_s.ip(),
            endpoints.remote_c.ip(),
            theirs.spi_c,
        ),
        (endpoints.remote_c.ip(), endpoints.local_s.ip(), ours.spi_s),
        (endpoints.remote_s.ip(), endpoints.local_c.ip(), ours.spi_c),
    ]
}

fn validate_endpoints(endpoints: &GmEndpoints) -> BridgeResult<()> {
    if endpoints.local_c.ip() != endpoints.local_s.ip()
        || endpoints.remote_c.ip() != endpoints.remote_s.ip()
        || endpoints.local_c.port() == 0
        || endpoints.local_s.port() == 0
        || endpoints.remote_c.port() == 0
        || endpoints.remote_s.port() == 0
        || endpoints.local_c == endpoints.local_s
        || endpoints.remote_c == endpoints.remote_s
    {
        return Err(BridgeError::Ims(
            "invalid Gm protected port endpoints".into(),
        ));
    }
    Ok(())
}

fn validate_rollover_endpoints(
    endpoints: &GmEndpoints,
    previous: &GmEndpoints,
) -> BridgeResult<()> {
    validate_endpoints(endpoints)?;
    validate_endpoints(previous)?;
    if endpoints.local_s != previous.local_s || endpoints.remote_s != previous.remote_s {
        return Err(BridgeError::Ims(
            "Gm reauthentication must preserve both protected server endpoints".into(),
        ));
    }
    if endpoints.local_c == previous.local_c || endpoints.remote_c == previous.remote_c {
        return Err(BridgeError::Ims(
            "Gm reauthentication must allocate distinct protected client ports".into(),
        ));
    }
    Ok(())
}

struct XfrmChange {
    apply: Vec<String>,
    undo: Vec<String>,
}

/// Run a reversible kernel update. A failed stage/activation leaves the old
/// generation usable; cleanup failures are logged without exposing key data.
fn apply_changes(
    changes: &[XfrmChange],
    run: &mut impl FnMut(&[String]) -> BridgeResult<()>,
) -> BridgeResult<()> {
    for (index, change) in changes.iter().enumerate() {
        if let Err(error) = run(&change.apply) {
            for applied in changes[..index].iter().rev() {
                if let Err(rollback_error) = run(&applied.undo) {
                    tracing::warn!(error = %rollback_error, "failed to roll back Gm IPsec update");
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn install_gm_sas_with_runner(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    ik: &[u8],
    ck: &[u8],
    previous: Option<(&GmEndpoints, &SaProposal, &SecurityServerParams)>,
    run: &mut impl FnMut(&[String]) -> BridgeResult<()>,
) -> BridgeResult<()> {
    validate_endpoints(endpoints)?;
    let state_ids = gm_state_ids(endpoints, ours, theirs);
    if state_ids[0] == state_ids[1] || state_ids[2] == state_ids[3] {
        return Err(BridgeError::Ims(
            "Gm SPI values must be distinct within a direction".into(),
        ));
    }
    let previous_policies = if let Some((old_endpoints, old_ours, old_theirs)) = previous {
        validate_rollover_endpoints(endpoints, old_endpoints)?;
        let old_states = gm_state_ids(old_endpoints, old_ours, old_theirs);
        if state_ids.iter().any(|state| old_states.contains(state)) {
            return Err(BridgeError::Ims(
                "Gm reauthentication must allocate new SPI values".into(),
            ));
        }
        gm_policies(old_endpoints, old_theirs)
    } else {
        Vec::new()
    };
    let auth_name = kernel_auth_name(&theirs.alg)?;
    let enc_name = kernel_cipher_name(&theirs.ealg)?;
    let auth_key = hex_encode(&derive_auth_key(&theirs.alg, ik)?);
    let enc_key = hex_encode(&derive_cipher_key(&theirs.ealg, ck)?);
    let mut changes = Vec::with_capacity(20);
    for (src, dst, spi) in state_ids {
        changes.push(XfrmChange {
            apply: xfrm_state_add_args(src, dst, spi, auth_name, &auth_key, enc_name, &enc_key),
            undo: xfrm_state_del_args(src, dst, spi),
        });
    }
    for policy in gm_policies(endpoints, theirs) {
        // The fixed server↔server selectors already exist. Inbound accepts
        // both generations; outbound must keep the old SPI until REGISTER's
        // 200 OK confirms the new security agreement.
        if previous_policies
            .iter()
            .any(|old| old.same_selector(&policy))
        {
            continue;
        }
        changes.push(XfrmChange {
            apply: xfrm_policy_args(&policy, "add"),
            undo: xfrm_policy_args(&policy, "delete"),
        });
    }
    apply_changes(&changes, run)
}

/// Install the four states and sixteen policies for an initial Gm security
/// agreement. A partial failure removes everything installed by this call.
pub fn install_gm_sas(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    proto: &str,
    ik: &[u8],
    ck: &[u8],
) -> BridgeResult<()> {
    // Both UDP and TCP are protected, independently of our client transport.
    let _ = proto;
    install_gm_sas_with_runner(endpoints, ours, theirs, ik, ck, None, &mut run_ip_xfrm)
}

/// Stage a new Gm generation alongside the existing one. Keep the existing
/// listener and both server ports; use new client ports and new SPI values.
/// The authenticated REGISTER can immediately use the new client leg while
/// established SIP transactions continue on the previous generation.
#[allow(clippy::too_many_arguments)]
pub fn install_gm_sas_for_reauth(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    proto: &str,
    ik: &[u8],
    ck: &[u8],
    previous_endpoints: &GmEndpoints,
    previous_ours: &SaProposal,
    previous_theirs: &SecurityServerParams,
) -> BridgeResult<()> {
    let _ = proto;
    install_gm_sas_with_runner(
        endpoints,
        ours,
        theirs,
        ik,
        ck,
        Some((previous_endpoints, previous_ours, previous_theirs)),
        &mut run_ip_xfrm,
    )
}

fn activate_gm_sas_with_runner(
    endpoints: &GmEndpoints,
    theirs: &SecurityServerParams,
    previous_endpoints: &GmEndpoints,
    previous_theirs: &SecurityServerParams,
    run: &mut impl FnMut(&[String]) -> BridgeResult<()>,
) -> BridgeResult<()> {
    validate_rollover_endpoints(endpoints, previous_endpoints)?;
    let mut changes = Vec::with_capacity(2);
    for proto in ["tcp", "udp"] {
        let mut policy = GmPolicy {
            src: endpoints.local_s,
            dst: endpoints.remote_s,
            spi: theirs.spi_s,
            inbound: false,
            proto,
        };
        let apply = xfrm_policy_args(&policy, "update");
        policy.spi = previous_theirs.spi_s;
        changes.push(XfrmChange {
            apply,
            undo: xfrm_policy_args(&policy, "update"),
        });
    }
    apply_changes(&changes, run)
}

/// After the authenticated REGISTER succeeds, switch the two shared outbound
/// server-port selectors to the new generation. Other old selectors and all
/// old states remain installed until their SIP transactions have drained.
/// An activation failure restores every policy changed by this call.
pub fn activate_gm_sas_after_reauth(
    endpoints: &GmEndpoints,
    theirs: &SecurityServerParams,
    proto: &str,
    previous_endpoints: &GmEndpoints,
    previous_theirs: &SecurityServerParams,
) -> BridgeResult<()> {
    let _ = proto;
    activate_gm_sas_with_runner(
        endpoints,
        theirs,
        previous_endpoints,
        previous_theirs,
        &mut run_ip_xfrm,
    )
}

fn remove_gm_sas_with_runner(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    keep_endpoints: Option<&GmEndpoints>,
    run: &mut impl FnMut(&[String]) -> BridgeResult<()>,
) {
    let keep_policies = keep_endpoints
        .map(|keep| gm_policies(keep, theirs))
        .unwrap_or_default();
    for policy in gm_policies(endpoints, theirs) {
        if keep_policies.iter().any(|keep| keep.same_selector(&policy)) {
            continue;
        }
        if let Err(error) = run(&xfrm_policy_args(&policy, "delete")) {
            tracing::warn!(error = %error, "failed to clean up Gm IPsec policy");
        }
    }
    for (src, dst, spi) in gm_state_ids(endpoints, ours, theirs) {
        if let Err(error) = run(&xfrm_state_del_args(src, dst, spi)) {
            tracing::warn!(error = %error, "failed to clean up Gm IPsec state");
        }
    }
}

/// Abort a staged generation or retire a drained previous generation without
/// deleting the server-port policies shared with the surviving generation.
/// To abort after activation, restore the previous shared outbound policy
/// first by calling `activate_gm_sas_after_reauth` with the generations swapped.
pub fn remove_gm_sas_preserving(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    proto: &str,
    keep_endpoints: &GmEndpoints,
) {
    let _ = proto;
    remove_gm_sas_with_runner(
        endpoints,
        ours,
        theirs,
        Some(keep_endpoints),
        &mut run_ip_xfrm,
    );
}

/// Best-effort cleanup of the final generation. Previous generations must be
/// retired with `remove_gm_sas_preserving` first, so their cleanup cannot
/// delete policies still required by this one.
pub fn remove_gm_sas(
    endpoints: &GmEndpoints,
    ours: &SaProposal,
    theirs: &SecurityServerParams,
    proto: &str,
) {
    let _ = proto;
    remove_gm_sas_with_runner(endpoints, ours, theirs, None, &mut run_ip_xfrm);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct TestXfrm {
        objects: BTreeMap<Vec<String>, Vec<String>>,
        calls: usize,
        fail_at: Option<usize>,
    }

    impl TestXfrm {
        fn run(&mut self, args: &[String]) -> BridgeResult<()> {
            self.calls += 1;
            if self.fail_at == Some(self.calls) {
                return Err(BridgeError::Ims("injected XFRM failure".into()));
            }
            let end = args
                .iter()
                .position(|arg| arg == if args[0] == "state" { "mode" } else { "tmpl" })
                .unwrap_or(args.len());
            let mut key = vec![args[0].clone()];
            key.extend_from_slice(&args[2..end]);
            match args[1].as_str() {
                "add" => {
                    if self.objects.contains_key(&key) {
                        return Err(BridgeError::Ims("duplicate XFRM object".into()));
                    }
                    self.objects.insert(key, args.to_vec());
                }
                "update" => {
                    if !self.objects.contains_key(&key) {
                        return Err(BridgeError::Ims("missing XFRM policy".into()));
                    }
                    // Normalize the action so restored objects compare equal.
                    let mut value = args.to_vec();
                    value[1] = "add".into();
                    self.objects.insert(key, value);
                }
                "delete" => {
                    if self.objects.remove(&key).is_none() {
                        return Err(BridgeError::Ims("missing XFRM object".into()));
                    }
                }
                action => panic!("unexpected XFRM action: {action}"),
            }
            Ok(())
        }
    }

    fn sa_generation(offset: u16) -> (GmEndpoints, SaProposal, SecurityServerParams) {
        let ours = SaProposal {
            spi_c: 1000 + u32::from(offset),
            spi_s: 2000 + u32::from(offset),
            port_c: 30000 + offset,
            port_s: 31000,
        };
        let theirs = SecurityServerParams {
            alg: "hmac-md5-96".into(),
            ealg: "null".into(),
            spi_c: 3000 + u32::from(offset),
            spi_s: 4000 + u32::from(offset),
            port_c: 32000 + offset,
            port_s: 33000,
            q: 1.0,
        };
        let endpoints = GmEndpoints::new(
            "192.0.2.1".parse().unwrap(),
            "192.0.2.2".parse().unwrap(),
            &ours,
            &theirs,
        );
        (endpoints, ours, theirs)
    }

    #[test]
    fn reauthentication_preserves_both_generations_until_old_transactions_drain() {
        let (old_endpoints, old_ours, old_theirs) = sa_generation(0);
        let (new_endpoints, new_ours, new_theirs) = sa_generation(1);
        let mut kernel = TestXfrm::default();
        install_gm_sas_with_runner(
            &old_endpoints,
            &old_ours,
            &old_theirs,
            &[1; 16],
            &[2; 16],
            None,
            &mut |args| kernel.run(args),
        )
        .unwrap();
        let initial_objects = kernel.objects.clone();
        assert_eq!(initial_objects.len(), 20);
        install_gm_sas_with_runner(
            &new_endpoints,
            &new_ours,
            &new_theirs,
            &[3; 16],
            &[4; 16],
            Some((&old_endpoints, &old_ours, &old_theirs)),
            &mut |args| kernel.run(args),
        )
        .unwrap();
        assert_eq!(kernel.objects.len(), 36);
        for (key, old_value) in &initial_objects {
            assert_eq!(
                kernel.objects.get(key),
                Some(old_value),
                "staging must preserve old SAs and policies"
            );
        }
        activate_gm_sas_with_runner(
            &new_endpoints,
            &new_theirs,
            &old_endpoints,
            &old_theirs,
            &mut |args| kernel.run(args),
        )
        .unwrap();
        // All old states must survive activation, including the one for
        // transactions still arriving on the old client connection.
        for (key, old_value) in &initial_objects {
            if key[0] == "state" {
                assert_eq!(kernel.objects.get(key), Some(old_value));
            }
        }
        remove_gm_sas_with_runner(
            &old_endpoints,
            &old_ours,
            &old_theirs,
            Some(&new_endpoints),
            &mut |args| kernel.run(args),
        );
        assert_eq!(kernel.objects.len(), 20);
        let mut expected = TestXfrm::default();
        install_gm_sas_with_runner(
            &new_endpoints,
            &new_ours,
            &new_theirs,
            &[3; 16],
            &[4; 16],
            None,
            &mut |args| expected.run(args),
        )
        .unwrap();
        assert_eq!(
            kernel.objects, expected.objects,
            "retiring the old generation must retain every new policy"
        );
    }

    #[test]
    fn failed_reauthentication_stage_rolls_back_every_partial_install() {
        let (old_endpoints, old_ours, old_theirs) = sa_generation(0);
        let (new_endpoints, new_ours, new_theirs) = sa_generation(1);
        // Four states and twelve selectors are unique to the staged set.
        for failure in 1..=16 {
            let mut kernel = TestXfrm::default();
            install_gm_sas_with_runner(
                &old_endpoints,
                &old_ours,
                &old_theirs,
                &[1; 16],
                &[2; 16],
                None,
                &mut |args| kernel.run(args),
            )
            .unwrap();
            let before = kernel.objects.clone();
            kernel.fail_at = Some(kernel.calls + failure);
            assert!(install_gm_sas_with_runner(
                &new_endpoints,
                &new_ours,
                &new_theirs,
                &[3; 16],
                &[4; 16],
                Some((&old_endpoints, &old_ours, &old_theirs)),
                &mut |args| kernel.run(args),
            )
            .is_err());
            assert_eq!(
                kernel.objects, before,
                "failed staging command {failure} must leave the previous generation intact"
            );
        }
    }

    #[test]
    fn aborting_staged_generation_preserves_the_previous_security_agreement() {
        let (old_endpoints, old_ours, old_theirs) = sa_generation(0);
        let (new_endpoints, new_ours, new_theirs) = sa_generation(1);
        let mut kernel = TestXfrm::default();
        install_gm_sas_with_runner(
            &old_endpoints,
            &old_ours,
            &old_theirs,
            &[1; 16],
            &[2; 16],
            None,
            &mut |args| kernel.run(args),
        )
        .unwrap();
        let before = kernel.objects.clone();
        install_gm_sas_with_runner(
            &new_endpoints,
            &new_ours,
            &new_theirs,
            &[3; 16],
            &[4; 16],
            Some((&old_endpoints, &old_ours, &old_theirs)),
            &mut |args| kernel.run(args),
        )
        .unwrap();
        remove_gm_sas_with_runner(
            &new_endpoints,
            &new_ours,
            &new_theirs,
            Some(&old_endpoints),
            &mut |args| kernel.run(args),
        );
        assert_eq!(kernel.objects, before);
        remove_gm_sas_with_runner(&old_endpoints, &old_ours, &old_theirs, None, &mut |args| {
            kernel.run(args)
        });
        assert!(kernel.objects.is_empty());
    }

    #[test]
    fn failed_activation_restores_shared_outbound_policy() {
        let (old_endpoints, old_ours, old_theirs) = sa_generation(0);
        let (new_endpoints, new_ours, new_theirs) = sa_generation(1);
        for failure in 1..=2 {
            let mut kernel = TestXfrm::default();
            install_gm_sas_with_runner(
                &old_endpoints,
                &old_ours,
                &old_theirs,
                &[1; 16],
                &[2; 16],
                None,
                &mut |args| kernel.run(args),
            )
            .unwrap();
            install_gm_sas_with_runner(
                &new_endpoints,
                &new_ours,
                &new_theirs,
                &[3; 16],
                &[4; 16],
                Some((&old_endpoints, &old_ours, &old_theirs)),
                &mut |args| kernel.run(args),
            )
            .unwrap();
            let before = kernel.objects.clone();
            kernel.fail_at = Some(kernel.calls + failure);
            assert!(activate_gm_sas_with_runner(
                &new_endpoints,
                &new_theirs,
                &old_endpoints,
                &old_theirs,
                &mut |args| kernel.run(args),
            )
            .is_err());
            assert_eq!(kernel.objects, before);
        }
    }

    #[test]
    fn inbound_policies_require_esp_and_accept_both_spi_generations() {
        let (endpoints, _, theirs) = sa_generation(0);
        for policy in gm_policies(&endpoints, &theirs)
            .into_iter()
            .filter(|policy| policy.inbound)
        {
            assert_eq!(policy.spi, 0);
            let args = xfrm_policy_args(&policy, "add");
            assert!(args.windows(2).any(|pair| pair == ["proto", "esp"]));
            assert!(
                !args.iter().any(|arg| arg == "optional"),
                "ESP authentication must remain mandatory"
            );
        }
    }

    #[test]
    fn reauthentication_rejects_changed_server_or_reused_client_ports() {
        let (old_endpoints, old_ours, old_theirs) = sa_generation(0);
        let (endpoints, ours, theirs) = sa_generation(1);
        for field in ["local_s", "remote_s", "local_c", "remote_c"] {
            let mut invalid = endpoints.clone();
            match field {
                "local_s" => invalid.local_s.set_port(12345),
                "remote_s" => invalid.remote_s.set_port(12345),
                "local_c" => invalid.local_c = old_endpoints.local_c,
                "remote_c" => invalid.remote_c = old_endpoints.remote_c,
                _ => unreachable!(),
            }
            let mut calls = 0;
            assert!(
                install_gm_sas_with_runner(
                    &invalid,
                    &ours,
                    &theirs,
                    &[3; 16],
                    &[4; 16],
                    Some((&old_endpoints, &old_ours, &old_theirs)),
                    &mut |_| {
                        calls += 1;
                        Ok(())
                    },
                )
                .is_err(),
                "must reject invalid {field}"
            );
            assert_eq!(calls, 0, "invalid negotiation must not mutate kernel state");
        }
    }

    #[test]
    fn xfrm_diagnostics_redact_aka_keys() {
        let args = xfrm_state_add_args(
            "192.0.2.1".parse().unwrap(),
            "192.0.2.2".parse().unwrap(),
            1234,
            "sha1",
            "secret-auth-key",
            "cbc(aes)",
            "secret-cipher-key",
        );
        let diagnostic = redacted_xfrm_args(&args).join(" ");
        assert!(!diagnostic.contains("secret-auth-key"));
        assert!(!diagnostic.contains("secret-cipher-key"));
        let stderr = redact_xfrm_stderr(&args, "bad keys 0xsecret-auth-key / 0xsecret-cipher-key");
        assert!(!stderr.contains("secret-auth-key"));
        assert!(!stderr.contains("secret-cipher-key"));
    }

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
}
