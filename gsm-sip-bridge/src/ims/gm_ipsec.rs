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
            "q" => q = value.parse::<f32>().unwrap_or(0.0),
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

#[allow(clippy::too_many_arguments)]
fn xfrm_policy_add(
    src: SocketAddr,
    dst: SocketAddr,
    tmpl_spi: u32,
    dir_in: bool,
    proto: &str,
) -> BridgeResult<()> {
    let dir = if dir_in { "in" } else { "out" };
    let proto_num = ip_proto_number(proto)?;
    run_ip_xfrm(&[
        "policy".to_string(),
        "add".to_string(),
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
        "spi".to_string(),
        format!("0x{tmpl_spi:08x}"),
        "mode".to_string(),
        "transport".to_string(),
    ])
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
    let auth_name = kernel_auth_name(&theirs.alg)?;
    let enc_name = kernel_cipher_name(&theirs.ealg)?;
    let auth_key = hex_encode(&derive_auth_key(&theirs.alg, ik)?);
    let enc_key = hex_encode(&derive_cipher_key(&theirs.ealg, ck)?);

    // TS 33.203 keys all four SAs identically from one `IK`.
    let in_auth_name = auth_name;
    let in_auth_key = auth_key.clone();

    // Outbound: we send, tagged with the SPI *they* told us to use.
    xfrm_state_add(
        endpoints.local_c.ip(),
        endpoints.remote_s.ip(),
        theirs.spi_s,
        auth_name,
        &auth_key,
        enc_name,
        &enc_key,
    )?;
    xfrm_state_add(
        endpoints.local_s.ip(),
        endpoints.remote_c.ip(),
        theirs.spi_c,
        auth_name,
        &auth_key,
        enc_name,
        &enc_key,
    )?;
    // Inbound: they send, tagged with the SPI *we* told them to use.
    xfrm_state_add(
        endpoints.remote_c.ip(),
        endpoints.local_s.ip(),
        ours.spi_s,
        in_auth_name,
        &in_auth_key,
        enc_name,
        &enc_key,
    )?;
    xfrm_state_add(
        endpoints.remote_s.ip(),
        endpoints.local_c.ip(),
        ours.spi_c,
        in_auth_name,
        &in_auth_key,
        enc_name,
        &enc_key,
    )?;

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
    // four selectors were.
    let _ = proto;
    for policy_proto in ["tcp", "udp"] {
        xfrm_policy_add(
            endpoints.local_c,
            endpoints.remote_s,
            theirs.spi_s,
            false,
            policy_proto,
        )?;
        xfrm_policy_add(
            endpoints.local_s,
            endpoints.remote_c,
            theirs.spi_c,
            false,
            policy_proto,
        )?;
        xfrm_policy_add(
            endpoints.remote_c,
            endpoints.local_s,
            ours.spi_s,
            true,
            policy_proto,
        )?;
        xfrm_policy_add(
            endpoints.remote_s,
            endpoints.local_c,
            ours.spi_c,
            true,
            policy_proto,
        )?;

        // The two canonical pairs above are what TS 33.203 draws, but they
        // only cover a client port talking to the *opposite* server port.
        // RFC 3261 §18.2.2 routes a response to the `Via` sent-by, and Jio's
        // P-CSCF sends network-initiated requests from its client port while
        // naming its *server* port in that `Via` — so the reply belongs on a
        // pairing the four canonical policies do not describe, and would
        // otherwise leave unprotected (silently: an unmatched policy is not an
        // error, the packet just goes out in the clear and the carrier drops
        // it).
        //
        // Completing the cross product costs four more selectors per protocol
        // and makes every combination of our two protected ports with their
        // two protected ports carry ESP, so response routing is free to follow
        // the SIP rules without a transport-layer trap underneath it.
        xfrm_policy_add(
            endpoints.local_c,
            endpoints.remote_c,
            theirs.spi_c,
            false,
            policy_proto,
        )?;
        xfrm_policy_add(
            endpoints.local_s,
            endpoints.remote_s,
            theirs.spi_s,
            false,
            policy_proto,
        )?;
        xfrm_policy_add(
            endpoints.remote_c,
            endpoints.local_c,
            ours.spi_c,
            true,
            policy_proto,
        )?;
        xfrm_policy_add(
            endpoints.remote_s,
            endpoints.local_s,
            ours.spi_s,
            true,
            policy_proto,
        )?;
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
    fn warn_on_err(label: &str, result: BridgeResult<()>) {
        if let Err(e) = result {
            tracing::warn!(what = label, error = %e, "failed to clean up Gm IPsec state");
        }
    }

    warn_on_err(
        "state local_c->remote_s",
        xfrm_state_del(
            endpoints.local_c.ip(),
            endpoints.remote_s.ip(),
            theirs.spi_s,
        ),
    );
    warn_on_err(
        "state local_s->remote_c",
        xfrm_state_del(
            endpoints.local_s.ip(),
            endpoints.remote_c.ip(),
            theirs.spi_c,
        ),
    );
    warn_on_err(
        "state remote_c->local_s",
        xfrm_state_del(endpoints.remote_c.ip(), endpoints.local_s.ip(), ours.spi_s),
    );
    warn_on_err(
        "state remote_s->local_c",
        xfrm_state_del(endpoints.remote_s.ip(), endpoints.local_c.ip(), ours.spi_c),
    );
    warn_on_err(
        "policy local_c->remote_s",
        xfrm_policy_del(endpoints.local_c, endpoints.remote_s, proto, false),
    );
    warn_on_err(
        "policy local_s->remote_c",
        xfrm_policy_del(endpoints.local_s, endpoints.remote_c, proto, false),
    );
    warn_on_err(
        "policy remote_c->local_s",
        xfrm_policy_del(endpoints.remote_c, endpoints.local_s, proto, true),
    );
    warn_on_err(
        "policy remote_s->local_c",
        xfrm_policy_del(endpoints.remote_s, endpoints.local_c, proto, true),
    );
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
