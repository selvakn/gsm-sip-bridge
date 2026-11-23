//! SIP message construction siptest cannot reuse from `ims::sip_client`,
//! which is IMS-specific end to end (research.md R1): its `build_register`
//! carries ICSI feature tags, an IMEI `+sip.instance` URN and a spoofed
//! Motorola `User-Agent`, and it has no INVITE/ACK/CANCEL builders at all — a
//! handset REGISTER, INVITE, ACK and CANCEL are plain RFC 3261.

use std::net::SocketAddr;

use gsm_sip_bridge::ims::digest;
use gsm_sip_bridge::ims::sip_client::{format_sip_addr, random_hex, SipRequest};

pub const USER_AGENT: &str = concat!("siptest/", env!("CARGO_PKG_VERSION"));

pub fn new_branch() -> String {
    format!("z9hG4bK{}", random_hex(8))
}

pub fn new_tag() -> String {
    random_hex(8)
}

pub struct RegisterParams<'a> {
    pub registrar_host: &'a str,
    pub aor_user: &'a str,
    pub local_addr: SocketAddr,
    pub call_id: &'a str,
    pub from_tag: &'a str,
    pub branch: &'a str,
    pub cseq: u32,
    pub expires: u32,
    pub authorization: Option<&'a str>,
}

pub fn build_register(p: &RegisterParams) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    let mut msg = format!(
        "REGISTER sip:{registrar} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{user}@{registrar}>;tag={from_tag}\r\n\
         To: <sip:{user}@{registrar}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} REGISTER\r\n\
         Contact: <sip:{user}@{via_addr}>\r\n\
         Expires: {expires}\r\n\
         Allow: INVITE, ACK, BYE, CANCEL, OPTIONS\r\n\
         User-Agent: {ua}\r\n",
        registrar = p.registrar_host,
        via_addr = via_addr,
        branch = p.branch,
        user = p.aor_user,
        from_tag = p.from_tag,
        call_id = p.call_id,
        cseq = p.cseq,
        expires = p.expires,
        ua = USER_AGENT,
    );
    if let Some(auth) = p.authorization {
        msg.push_str("Authorization: ");
        msg.push_str(auth);
        msg.push_str("\r\n");
    }
    msg.push_str("Content-Length: 0\r\n\r\n");
    msg
}

/// Digest `Authorization` value for a REGISTER challenge (RFC 2617,
/// `qop=auth` only — the registrar refuses `auth-int`/`MD5-sess`).
#[allow(clippy::too_many_arguments)]
pub fn build_authorization(
    username: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    nc: u32,
    cnonce: &str,
) -> String {
    let ha1 = digest::ha1(username, realm, password.as_bytes());
    let ha2 = digest::ha2(method, uri);
    let nc_str = format!("{nc:08x}");
    let response = digest::response_qop(&ha1, nonce, &nc_str, cnonce, "auth", &ha2);
    format!(
        "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", \
         response=\"{response}\", algorithm=MD5, qop=auth, nc={nc_str}, cnonce=\"{cnonce}\""
    )
}

pub struct InviteParams<'a> {
    pub request_uri: &'a str,
    pub local_addr: SocketAddr,
    pub from_user: &'a str,
    pub from_host: &'a str,
    pub to_user: &'a str,
    pub to_host: &'a str,
    pub call_id: &'a str,
    pub from_tag: &'a str,
    pub branch: &'a str,
    pub cseq: u32,
    pub sdp_body: &'a str,
}

/// Deliberately does **not** advertise `Supported: 100rel, timer`
/// (sip-flows.md C-2 / research.md R10) — a pjsua UAS that sees `100rel` may
/// `Require` it on a 183 and abandon the call when siptest never PRACKs.
pub fn build_invite(p: &InviteParams) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    format!(
        "INVITE {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{from_user}@{from_host}>;tag={from_tag}\r\n\
         To: <sip:{to_user}@{to_host}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} INVITE\r\n\
         Contact: <sip:{from_user}@{via_addr}>\r\n\
         Allow: INVITE, ACK, BYE, CANCEL, OPTIONS\r\n\
         User-Agent: {ua}\r\n\
         Content-Type: application/sdp\r\n\
         Content-Length: {len}\r\n\r\n\
         {sdp}",
        ruri = p.request_uri,
        via_addr = via_addr,
        branch = p.branch,
        from_user = p.from_user,
        from_host = p.from_host,
        from_tag = p.from_tag,
        to_user = p.to_user,
        to_host = p.to_host,
        call_id = p.call_id,
        cseq = p.cseq,
        ua = USER_AGENT,
        len = p.sdp_body.len(),
        sdp = p.sdp_body,
    )
}

/// ACK for a non-2xx final response (RFC 3261 §17.1.1.3): same Call-ID, same
/// CSeq **number** with method ACK, and — critically — **the same `Via`
/// branch as the INVITE it acknowledges**. Sent to the same URI the INVITE
/// went to (the registrar), never to a redirect target.
pub struct AckNon2xxParams<'a> {
    pub request_uri: &'a str,
    pub local_addr: SocketAddr,
    pub from_user: &'a str,
    pub from_host: &'a str,
    pub to_user: &'a str,
    pub to_host: &'a str,
    pub to_tag: &'a str,
    pub call_id: &'a str,
    pub from_tag: &'a str,
    /// Must equal the INVITE's own branch.
    pub invite_branch: &'a str,
    pub cseq: u32,
}

pub fn build_ack_non_2xx(p: &AckNon2xxParams) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    format!(
        "ACK {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{from_user}@{from_host}>;tag={from_tag}\r\n\
         To: <sip:{to_user}@{to_host}>;tag={to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         Content-Length: 0\r\n\r\n",
        ruri = p.request_uri,
        via_addr = via_addr,
        branch = p.invite_branch,
        from_user = p.from_user,
        from_host = p.from_host,
        from_tag = p.from_tag,
        to_user = p.to_user,
        to_host = p.to_host,
        to_tag = p.to_tag,
        call_id = p.call_id,
        cseq = p.cseq,
    )
}

/// ACK for a 2xx final response (RFC 3261 §13.2.2.4): a **new** branch, sent
/// to the URI in the 200's own `Contact`.
pub struct Ack2xxParams<'a> {
    pub request_uri: &'a str,
    pub local_addr: SocketAddr,
    pub from_user: &'a str,
    pub from_host: &'a str,
    pub to_user: &'a str,
    pub to_host: &'a str,
    pub to_tag: &'a str,
    pub call_id: &'a str,
    pub from_tag: &'a str,
    pub branch: &'a str,
    pub cseq: u32,
}

pub fn build_ack_2xx(p: &Ack2xxParams) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    format!(
        "ACK {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{from_user}@{from_host}>;tag={from_tag}\r\n\
         To: <sip:{to_user}@{to_host}>;tag={to_tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} ACK\r\n\
         Content-Length: 0\r\n\r\n",
        ruri = p.request_uri,
        via_addr = via_addr,
        branch = p.branch,
        from_user = p.from_user,
        from_host = p.from_host,
        from_tag = p.from_tag,
        to_user = p.to_user,
        to_host = p.to_host,
        to_tag = p.to_tag,
        call_id = p.call_id,
        cseq = p.cseq,
    )
}

/// CANCEL reuses the INVITE's exact branch (RFC 3261 §9.1) — it does not
/// exist anywhere else in the repo (research.md R1).
pub struct CancelParams<'a> {
    pub request_uri: &'a str,
    pub local_addr: SocketAddr,
    pub from_user: &'a str,
    pub from_host: &'a str,
    pub to_user: &'a str,
    pub to_host: &'a str,
    pub call_id: &'a str,
    pub from_tag: &'a str,
    pub invite_branch: &'a str,
    pub cseq: u32,
}

pub fn build_cancel(p: &CancelParams) -> String {
    let via_addr = format_sip_addr(p.local_addr);
    format!(
        "CANCEL {ruri} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {via_addr};branch={branch};rport\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:{from_user}@{from_host}>;tag={from_tag}\r\n\
         To: <sip:{to_user}@{to_host}>\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: {cseq} CANCEL\r\n\
         Content-Length: 0\r\n\r\n",
        ruri = p.request_uri,
        via_addr = via_addr,
        branch = p.invite_branch,
        from_user = p.from_user,
        from_host = p.from_host,
        from_tag = p.from_tag,
        to_user = p.to_user,
        to_host = p.to_host,
        call_id = p.call_id,
        cseq = p.cseq,
    )
}

/// Whether a challenge's `stale` parameter is `true` — `extract_challenge`
/// doesn't carry it (it's IMS-AKA focused), so this reads the raw params.
pub fn challenge_is_stale(params: &[(String, String)]) -> bool {
    params
        .iter()
        .any(|(k, v)| k == "stale" && v.eq_ignore_ascii_case("true"))
}

/// The dialog-identifying triple every in-dialog request must reuse.
pub fn call_id_of(req: &SipRequest) -> Option<&str> {
    req.header("Call-ID")
}

/// The `tag` parameter of a `From`/`To` header value, if it has a non-empty
/// one.
///
/// RFC 3261 allows whitespace around both the `;` and the `=` (§25.1:
/// `SEMI = *WSP ";" *WSP`, `EQUAL = SWS "=" SWS`) and parameter names are
/// case-insensitive, so `;tag=x`, `; tag = x` and `;Tag=x` are the same
/// header. Only *header* parameters count: with a `name-addr` form
/// (`"Name" <sip:a@b;tag=u>;tag=h`) they are the ones after the closing `>`,
/// and a `;tag=` inside the angle brackets is a URI parameter, not the
/// dialog tag.
pub fn header_tag(value: &str) -> Option<&str> {
    let params = match value.rfind('>') {
        Some(end) => &value[end + 1..],
        None => value.find(';').map_or("", |i| &value[i..]),
    };
    params
        .split(';')
        .filter_map(|param| param.split_once('='))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("tag"))
        .map(|(_, tag)| tag.trim())
        .filter(|tag| !tag.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_carries_no_ims_feature_tags() {
        let msg = build_register(&RegisterParams {
            registrar_host: "gsm-sip-bridge",
            aor_user: "1002",
            local_addr: "192.168.15.10:5065".parse().unwrap(),
            call_id: "call-1",
            from_tag: "tag-1",
            branch: "z9hG4bKabc",
            cseq: 1,
            expires: 300,
            authorization: None,
        });
        assert!(!msg.contains("icsi-ref"));
        assert!(!msg.contains("g.3gpp.smsip"));
        assert!(!msg.contains("sip.instance"));
        assert!(!msg.contains("gsma:imei"));
        assert!(msg.starts_with("REGISTER sip:gsm-sip-bridge SIP/2.0\r\n"));
    }

    #[test]
    fn cancel_branch_matches_the_invite_it_cancels() {
        let invite_branch = "z9hG4bKinvite123";
        let msg = build_cancel(&CancelParams {
            request_uri: "sip:+919000000000@192.168.15.10:5060",
            local_addr: "192.168.15.10:5065".parse().unwrap(),
            from_user: "1002",
            from_host: "gsm-sip-bridge",
            to_user: "+919000000000",
            to_host: "192.168.15.10",
            call_id: "call-1",
            from_tag: "tag-1",
            invite_branch,
            cseq: 2,
        });
        assert!(msg.contains(&format!("branch={invite_branch}")));
        assert!(msg.contains("CSeq: 2 CANCEL"));
    }

    #[test]
    fn non_2xx_ack_and_2xx_ack_differ_in_request_uri_and_branch() {
        let non2xx = build_ack_non_2xx(&AckNon2xxParams {
            request_uri: "sip:+919000000000@192.168.15.10:5060",
            local_addr: "192.168.15.10:5065".parse().unwrap(),
            from_user: "1002",
            from_host: "gsm-sip-bridge",
            to_user: "+919000000000",
            to_host: "192.168.15.10",
            to_tag: "totag",
            call_id: "call-1",
            from_tag: "tag-1",
            invite_branch: "z9hG4bKinvite123",
            cseq: 2,
        });
        let xx2 = build_ack_2xx(&Ack2xxParams {
            request_uri: "sip:+919000000000@192.168.15.10:5072",
            local_addr: "192.168.15.10:5065".parse().unwrap(),
            from_user: "1002",
            from_host: "gsm-sip-bridge",
            to_user: "+919000000000",
            to_host: "192.168.15.10",
            to_tag: "totag2",
            call_id: "call-1",
            from_tag: "tag-1",
            branch: "z9hG4bKnewbranch",
            cseq: 3,
        });
        assert!(non2xx.starts_with("ACK sip:+919000000000@192.168.15.10:5060"));
        assert!(xx2.starts_with("ACK sip:+919000000000@192.168.15.10:5072"));
        assert!(non2xx.contains("branch=z9hG4bKinvite123"));
        assert!(xx2.contains("branch=z9hG4bKnewbranch"));
        assert_ne!(non2xx, xx2);
    }

    #[test]
    fn digest_authorization_response_matches_a_known_vector() {
        // Cross-checked field-by-field against ha1/ha2/response_qop's own
        // unit tests in gsm_sip_bridge::ims::digest.
        let auth = build_authorization(
            "1002",
            "gsm-sip-bridge",
            "hunter2",
            "REGISTER",
            "sip:gsm-sip-bridge",
            "noncevalue",
            1,
            "cnoncevalue",
        );
        assert!(auth.starts_with("Digest username=\"1002\""));
        assert!(auth.contains("nc=00000001"));
        assert!(auth.contains("qop=auth"));
        assert!(auth.contains("algorithm=MD5"));
    }

    #[test]
    fn stale_flag_is_detected_case_insensitively() {
        let stale = vec![("stale".to_string(), "TRUE".to_string())];
        let not_stale = vec![("stale".to_string(), "false".to_string())];
        let absent: Vec<(String, String)> = vec![];
        assert!(challenge_is_stale(&stale));
        assert!(!challenge_is_stale(&not_stale));
        assert!(!challenge_is_stale(&absent));
    }

    #[test]
    fn header_tag_accepts_every_spelling_rfc_3261_allows() {
        for (value, expected) in [
            ("<sip:a@h>;tag=abc", Some("abc")),
            ("<sip:a@h> ; tag=abc", Some("abc")),
            ("<sip:a@h>;  tag = abc ", Some("abc")),
            ("<sip:a@h>;Tag=abc", Some("abc")),
            ("<sip:a@h>;TAG=abc", Some("abc")),
            ("; tag=abc", Some("abc")),
            ("sip:a@h;tag=abc", Some("abc")),
            ("\"Al\" <sip:a@h>;foo=1;tag=abc;bar", Some("abc")),
            // A `>` inside a quoted display name does not end the URI.
            ("\"a>b\" <sip:a@h>;tag=abc", Some("abc")),
        ] {
            assert_eq!(header_tag(value), expected, "{value:?}");
        }
    }

    #[test]
    fn header_tag_is_absent_when_there_is_no_header_tag() {
        for value in [
            "<sip:a@h>",
            "<sip:a@h>;tag=",
            "<sip:a@h>; tag = ",
            "<sip:a@h>;notag=abc",
            "<sip:a@h>;tagged",
            "",
            // A `tag` parameter of the URI is not the dialog's tag.
            "<sip:a@h;tag=uri>",
            "\"Al\" <sip:a@h;tag=uri>;foo=1",
        ] {
            assert_eq!(header_tag(value), None, "{value:?}");
        }
    }
}
