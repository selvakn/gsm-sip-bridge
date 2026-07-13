//! Real Gm IPsec SA setup (TS 33.203 Annex H) — installs kernel XFRM states
//! and policies so the authenticated REGISTER can go out over an actual
//! IPsec-protected connection to the network's negotiated port, matching
//! what a real UE (and sysmocom's `volte.c`) does after a `Security-Server`
//! response comes back. See `docs/gm-ipsec-xfrm-plan.md` for the derivation
//! of this topology from a captured working registration.
//!
//! Shells out to `ip xfrm` rather than speaking raw netlink, to stay
//! consistent with this crate's zero-`unsafe` policy and the pattern
//! `docker/entrypoint.sh` already uses for netns/route setup.

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
}

pub fn parse_security_server(header: &str) -> BridgeResult<SecurityServerParams> {
    let mut alg = None;
    let mut ealg = None;
    let mut spi_c = None;
    let mut spi_s = None;
    let mut port_c = None;
    let mut port_s = None;

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
        auth_name,
        &auth_key,
        enc_name,
        &enc_key,
    )?;
    xfrm_state_add(
        endpoints.remote_s.ip(),
        endpoints.local_c.ip(),
        ours.spi_c,
        auth_name,
        &auth_key,
        enc_name,
        &enc_key,
    )?;

    xfrm_policy_add(
        endpoints.local_c,
        endpoints.remote_s,
        theirs.spi_s,
        false,
        proto,
    )?;
    xfrm_policy_add(
        endpoints.local_s,
        endpoints.remote_c,
        theirs.spi_c,
        false,
        proto,
    )?;
    xfrm_policy_add(
        endpoints.remote_c,
        endpoints.local_s,
        ours.spi_s,
        true,
        proto,
    )?;
    xfrm_policy_add(
        endpoints.remote_s,
        endpoints.local_c,
        ours.spi_c,
        true,
        proto,
    )?;

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
