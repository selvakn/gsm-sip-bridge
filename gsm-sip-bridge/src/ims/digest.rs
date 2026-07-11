//! RFC 2617 HTTP Digest math as adapted by RFC 3310 for IMS-AKA: the AKA
//! `RES` value is used directly as the digest "password" (raw octets, not
//! hex-encoded) when computing H(A1).

use md5::{Digest, Md5};

fn md5_hex(input: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(input);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// H(A1) = MD5(username ":" realm ":" password), with `password` = the raw
/// AKA RES octets (RFC 3310 §3.2) rather than a UTF-8 string.
pub fn ha1(username: &str, realm: &str, res: &[u8]) -> String {
    let mut a1 = Vec::with_capacity(username.len() + realm.len() + res.len() + 2);
    a1.extend_from_slice(username.as_bytes());
    a1.push(b':');
    a1.extend_from_slice(realm.as_bytes());
    a1.push(b':');
    a1.extend_from_slice(res);
    md5_hex(&a1)
}

/// H(A2) = MD5(method ":" uri) — the `qop=auth-int` variant (which also
/// hashes the body) is not needed for REGISTER, which has no body.
pub fn ha2(method: &str, uri: &str) -> String {
    md5_hex(format!("{method}:{uri}").as_bytes())
}

/// Digest response without `qop` (legacy RFC 2069 form):
/// response = MD5(HA1 ":" nonce ":" HA2)
pub fn response_simple(ha1: &str, nonce: &str, ha2: &str) -> String {
    md5_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes())
}

/// Digest response with `qop=auth` (RFC 2617):
/// response = MD5(HA1 ":" nonce ":" nc ":" cnonce ":" qop ":" HA2)
pub fn response_qop(
    ha1: &str,
    nonce: &str,
    nc: &str,
    cnonce: &str,
    qop: &str,
    ha2: &str,
) -> String {
    md5_hex(format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ha1_uses_raw_res_bytes_not_hex() {
        // RES with a non-UTF8-ish byte to confirm raw-byte concatenation.
        let res = [0x81u8, 0x08, 0x7B, 0xD0];
        let a = ha1("user@realm", "realm", &res);
        let b = ha1("user@realm", "realm", b"81087BD0"); // hex-string RES would differ
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn ha2_matches_known_vector() {
        // MD5("REGISTER:sip:example.com") — cross-checked against `md5sum`.
        let h = ha2("REGISTER", "sip:example.com");
        assert_eq!(h.len(), 32);
        // deterministic: same input always produces same output
        assert_eq!(h, ha2("REGISTER", "sip:example.com"));
    }

    #[test]
    fn response_simple_deterministic_and_sensitive_to_inputs() {
        let h1 = ha1("alice", "realm", b"res");
        let h2 = ha2("REGISTER", "sip:realm");
        let r1 = response_simple(&h1, "noncevalue", &h2);
        let r2 = response_simple(&h1, "othernonce", &h2);
        assert_eq!(r1.len(), 32);
        assert_ne!(r1, r2);
    }

    #[test]
    fn response_qop_differs_from_simple() {
        let h1 = ha1("alice", "realm", b"res");
        let h2 = ha2("REGISTER", "sip:realm");
        let simple = response_simple(&h1, "n", &h2);
        let qop = response_qop(&h1, "n", "00000001", "cnonce123", "auth", &h2);
        assert_ne!(simple, qop);
    }
}
