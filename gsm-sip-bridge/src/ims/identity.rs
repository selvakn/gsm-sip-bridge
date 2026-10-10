//! Reading a caller's identity out of SIP headers, per the grammars that
//! actually govern them — RFC 3261 §25.1 (`name-addr` / `addr-spec`, and the
//! comma-separated list a header line may hold), RFC 3325 §9.1
//! (`P-Asserted-Identity`: one or two values, `sip`/`sips`/`tel`) and
//! RFC 3966 §3 (the `tel:` URI).
//!
//! Pure functions over text and [`SipRequest`]; no I/O. Pinned by
//! `specs/083-tel-uri-caller-id/contracts/identity-parsing.md` — every row of
//! that contract is a test below.

use super::sip_client::SipRequest;

/// Split one header value on the commas that separate entries, ignoring
/// commas inside `<...>` (a URI may contain them) or inside a quoted display
/// name (`"Bob, Smith" <sip:...>`, with `\"` escapes).
///
/// RFC 3261 §7.3.1 makes repeated header lines equivalent to one
/// comma-joined line, so callers feed every line through this.
pub(crate) fn split_header_values(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    let (mut quoted, mut escaped) = (false, false);
    for (i, ch) in value.char_indices() {
        if quoted {
            match (escaped, ch) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => quoted = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => quoted = true,
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(value[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(value[start..].trim());
    out.retain(|e| !e.is_empty());
    out
}

/// Whether an unbracketed `addr-spec`'s `;params` are header parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderParams {
    /// `From`, `To`, `Contact`: without `<>`, every `;param` after the URI is
    /// a *header* parameter (RFC 3261 §20.10, §20.20).
    Allowed,
    /// `P-Asserted-Identity`: RFC 3325 §9.1's grammar defines no header
    /// parameters, so a `;param` can only be part of the URI.
    None,
}

/// One parsed header value: an optional display name and its URI.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NameAddr<'a> {
    pub display: Option<String>,
    pub uri: &'a str,
}

/// Parses one header value as a `name-addr` (`[display-name] <URI>`) or a
/// bare `addr-spec`; `None` when it names no URI at all (`"Anonymous"`), or
/// holds an unterminated quoted-string.
///
/// A quoted display name ends at its closing `"`, tracking `\`-escapes — it
/// may legitimately contain `<` and `,` (RFC 3261 §25.1's `qdtext` excludes
/// only `"` and `\`), so the URI is *not* located with a plain
/// `split_once('<')`. Only a bare token display name, which the grammar
/// forbids from containing `<`, may use the character itself as the boundary.
///
/// The display name is `None` when absent, empty, or containing a bare CR/LF
/// (never legitimate; rejected here rather than passed on to become a
/// header-injection vector in whatever onward request re-presents it). Absence
/// is a real, common outcome (the Nokia SBC's `X-P-Asserted-Identity` carries
/// no display name at all) and never collapses to a placeholder string.
pub(crate) fn parse_name_addr(value: &str, params: HeaderParams) -> Option<NameAddr<'_>> {
    let value = value.trim();
    let (display, uri) = if let Some(after) = value.strip_prefix('"') {
        let mut name = String::new();
        let mut escaped = false;
        let mut end = None;
        for (i, c) in after.char_indices() {
            if escaped {
                name.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                end = Some(i + c.len_utf8());
                break;
            } else {
                name.push(c);
            }
        }
        let rest = after[end?..].trim_start().strip_prefix('<')?;
        (name, &rest[..rest.find('>')?])
    } else if let Some(lt) = value.find('<') {
        let rest = &value[lt + 1..];
        (value[..lt].trim().to_string(), &rest[..rest.find('>')?])
    } else {
        // A bare `addr-spec`: with no brackets to mark the end of the URI,
        // whether a `;` is the URI's own depends on the header (§20.10).
        let uri = match params {
            HeaderParams::Allowed => value.split(';').next().unwrap_or(value),
            HeaderParams::None => value,
        };
        (String::new(), uri)
    };
    let uri = uri.trim();
    if !uri.contains(':') {
        return None;
    }
    let display = (!display.is_empty() && !display.contains(['\r', '\n'])).then_some(display);
    Some(NameAddr { display, uri })
}

/// `%HH`-decodes `s`; `None` for a `%` not followed by two hex digits, or a
/// result that is not UTF-8 (RFC 3261 §19.1.4: an escape is equivalent to
/// the character it encodes).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The number a URI names, or `None` when it names none.
///
/// - `tel` (RFC 3966 §3): the telephone-subscriber before the first `;`;
///   every parameter (`ext`, `isub`, `phone-context`, and vendor ones such as
///   `noa`/`srvattri`) follows a `;`. A local number is reported as dialled —
///   RFC 3966 does not define combining it with its `phone-context`.
/// - `sip`/`sips` (RFC 3261 §25.1): the user part, which exists only when the
///   URI has an `@` — without one the URI names a host, not a person. The
///   first `@` ends it (it is not legal unescaped anywhere else in the URI),
///   a `:password` is dropped, and so are telephone-subscriber parameters
///   (`;npdi;rn=…`), which `user` may carry after a `;`.
/// - Any other scheme: `None`. Schemes are case-insensitive (RFC 3986 §3.1).
///
/// The raw part is then percent-decoded; for a phone number — `tel`,
/// `user=phone`, or a `+`-leading user — the visual separators `-.()` are
/// removed (RFC 3966 §5.1.1). Finally only RFC 3986 unreserved characters
/// plus `+*#` are accepted: the result is embedded in `<tel:{caller}>`,
/// `X-GSM-Caller-ID`, CDRs and logs, so an escape that decodes to `>`, `"`,
/// `,`, `;`, whitespace or CR/LF must never get through.
pub(crate) fn uri_number(uri: &str) -> Option<String> {
    let (scheme, rest) = uri.trim().split_once(':')?;
    let (raw, is_phone) = if scheme.eq_ignore_ascii_case("tel") {
        (rest.split(';').next()?, true)
    } else if scheme.eq_ignore_ascii_case("sip") || scheme.eq_ignore_ascii_case("sips") {
        let (userinfo, hostpart) = rest.split_once('@')?;
        let user = userinfo.split(';').next()?.split(':').next()?;
        let user_phone = hostpart
            .split('?')
            .next()
            .unwrap_or_default()
            .split(';')
            .any(|p| p.trim().eq_ignore_ascii_case("user=phone"));
        let leading_plus =
            user.starts_with('+') || user.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("%2B"));
        (user, user_phone || leading_plus)
    } else {
        return None;
    };
    let mut number = percent_decode(raw)?;
    if is_phone {
        number.retain(|c| !matches!(c, '-' | '.' | '(' | ')'));
    }
    let safe = !number.is_empty()
        && number.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '+' | '*' | '#' | '-' | '.' | '_' | '~')
        });
    safe.then_some(number)
}

/// The caller as taken from one header.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub number: String,
    pub display: Option<String>,
}

/// Every parsable value of every line of the named header, in order
/// (RFC 3261 §7.3.1: repeated lines are one comma-joined list).
pub(crate) fn header_uri_values<'a>(
    req: &'a SipRequest,
    name: &str,
    params: HeaderParams,
) -> Vec<NameAddr<'a>> {
    req.headers_all(name)
        .into_iter()
        .flat_map(split_header_values)
        .filter_map(|v| parse_name_addr(v, params))
        .collect()
}

fn is_tel(uri: &str) -> bool {
    uri.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("tel"))
}

/// The caller named by one header, or `None` when none of its values yields a
/// number.
///
/// RFC 3325 §9.1 allows a `P-Asserted-Identity` two values — a `sip`/`sips`
/// and a `tel` — naming the same user, so the number comes from the `tel`
/// one when it yields one (the canonical form), else from the first value
/// that does. The display name is the first non-empty one among **this
/// header's** values, never reached for in another header: pairing a number
/// from one header with a name from the other would present a name that does
/// not belong to that number.
pub(crate) fn header_identity(
    req: &SipRequest,
    name: &str,
    params: HeaderParams,
) -> Option<Identity> {
    let values = header_uri_values(req, name, params);
    let number = values
        .iter()
        .filter(|v| is_tel(v.uri))
        .find_map(|v| uri_number(v.uri))
        .or_else(|| values.iter().find_map(|v| uri_number(v.uri)))?;
    let display = values.iter().find_map(|v| v.display.clone());
    Some(Identity { number, display })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(headers: &str) -> SipRequest {
        let raw = format!(
            "INVITE sip:x SIP/2.0\r\n{headers}Call-ID: c\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        );
        SipRequest::try_parse(raw.as_bytes()).unwrap().unwrap().0
    }

    #[test]
    fn split_header_values_ignores_commas_inside_quoted_display_names() {
        assert_eq!(
            split_header_values(r#""Bob, Smith" <sip:a;lr>, "Q \" , x" <sip:b;lr>"#),
            vec![r#""Bob, Smith" <sip:a;lr>"#, r#""Q \" , x" <sip:b;lr>"#]
        );
    }

    #[test]
    fn split_header_values_ignores_commas_inside_uris() {
        assert_eq!(
            split_header_values("<sip:a;x=1,2;lr>, <sip:b;lr>"),
            vec!["<sip:a;x=1,2;lr>", "<sip:b;lr>"]
        );
    }

    /// Contract C2: `name-addr` / `addr-spec` parsing.
    #[test]
    fn parse_name_addr_follows_the_contract_table() {
        use HeaderParams::{Allowed, None as NoParams};
        // (value, params, display, uri) — `uri: None` means the whole value
        // is rejected.
        let table: &[(&str, HeaderParams, Option<&str>, Option<&str>)] = &[
            (
                "<tel:+919000000000>",
                NoParams,
                None,
                Some("tel:+919000000000"),
            ),
            (
                r#""Asserted Name" <tel:+919000000000;cpc=ordinary>"#,
                NoParams,
                Some("Asserted Name"),
                Some("tel:+919000000000;cpc=ordinary"),
            ),
            (
                "Asserted Name <sip:+919000000000@ims.example>",
                NoParams,
                Some("Asserted Name"),
                Some("sip:+919000000000@ims.example"),
            ),
            (
                r#""Doe <Jr>, sip:x" <tel:+919000000000>"#,
                NoParams,
                Some("Doe <Jr>, sip:x"),
                Some("tel:+919000000000"),
            ),
            (
                r#""Q \"x\"" <tel:+919000000000>"#,
                NoParams,
                Some(r#"Q "x""#),
                Some("tel:+919000000000"),
            ),
            (
                "sip:ipsmgw.example;lr",
                NoParams,
                None,
                Some("sip:ipsmgw.example;lr"),
            ),
            (
                "sip:ipsmgw.example;tag=abc",
                Allowed,
                None,
                Some("sip:ipsmgw.example"),
            ),
            (
                "<sip:gw.example>;tag=abc",
                Allowed,
                None,
                Some("sip:gw.example"),
            ),
            (r#""Anonymous""#, NoParams, None, None),
            (
                r#""" <tel:+919000000000>"#,
                NoParams,
                None,
                Some("tel:+919000000000"),
            ),
            (r#""unterminated <tel:+919000000000>"#, NoParams, None, None),
        ];
        for (value, params, display, uri) in table {
            let got = parse_name_addr(value, *params);
            match uri {
                None => assert_eq!(got, None, "{value}"),
                Some(uri) => {
                    let got = got.unwrap_or_else(|| panic!("{value} should parse"));
                    assert_eq!(got.uri, *uri, "uri of {value}");
                    assert_eq!(got.display.as_deref(), *display, "display of {value}");
                }
            }
        }
    }

    /// A bare CR/LF in a display name is a header-injection vector: the name
    /// is dropped (the URI is still usable).
    #[test]
    fn parse_name_addr_rejects_a_display_name_with_a_line_break() {
        let got = parse_name_addr("\"a\r\nb\" <tel:+919000000000>", HeaderParams::None).unwrap();
        assert_eq!(got.display, None);
        assert_eq!(got.uri, "tel:+919000000000");
    }

    /// Contract C1: the number a URI names.
    #[test]
    fn uri_number_follows_the_contract_table() {
        let table: &[(&str, Option<&str>)] = &[
            (
                "tel:+919000000000;noa=international;srvattri=national",
                Some("+919000000000"),
            ),
            ("tel:+919000000000", Some("+919000000000")),
            ("TEL:+919000000000", Some("+919000000000")),
            ("tel:+91-900-000-0000", Some("+919000000000")),
            ("tel:+91(900)000.0000", Some("+919000000000")),
            ("tel:9000000000;phone-context=+91", Some("9000000000")),
            ("tel:+919000000000;ext=12", Some("+919000000000")),
            ("sip:+919000000000@ims.example", Some("+919000000000")),
            (
                "Sip:+919000000000@ims.example;user=phone",
                Some("+919000000000"),
            ),
            ("sips:+919000000000@ims.example", Some("+919000000000")),
            (
                "sip:+919000000000;npdi;rn=+919000000099@ims.example;user=phone",
                Some("+919000000000"),
            ),
            (
                "sip:+91-900-000-0000@ims.example;user=phone",
                Some("+919000000000"),
            ),
            ("sip:%2B919000000000@ims.example", Some("+919000000000")),
            ("sip:alice:secret@ims.example", Some("alice")),
            ("sip:A2P@203.0.113.7;transport=udp", Some("A2P")),
            ("sip:gateway.ims.example", None),
            ("sip:gateway.ims.example;transport=udp", None),
            ("sip:a%3E%0D%0Ab@ims.example", None),
            ("sip:a%22b@ims.example", None),
            ("sip:%zz@ims.example", None),
            ("tel:", None),
            ("mailto:a@example.com", None),
            ("urn:service:sos", None),
        ];
        for (uri, want) in table {
            assert_eq!(uri_number(uri).as_deref(), *want, "{uri}");
        }
    }

    #[test]
    fn header_identity_prefers_the_tel_value_for_the_number() {
        let r =
            req("P-Asserted-Identity: <sip:+919000000001@ims.example>, <tel:+919000000000>\r\n");
        let id = header_identity(&r, "P-Asserted-Identity", HeaderParams::None).unwrap();
        assert_eq!(id.number, "+919000000000");
    }

    #[test]
    fn header_identity_takes_the_name_from_another_value_of_the_same_header() {
        let r = req("P-Asserted-Identity: <tel:+919000000000>\r\n\
             P-Asserted-Identity: \"Asserted Name\" <sip:+919000000000@ims.example>\r\n");
        let id = header_identity(&r, "P-Asserted-Identity", HeaderParams::None).unwrap();
        assert_eq!(id.number, "+919000000000");
        assert_eq!(id.display.as_deref(), Some("Asserted Name"));
    }

    #[test]
    fn header_identity_is_none_when_no_value_yields_a_number() {
        let r = req("P-Asserted-Identity: <sip:gateway.ims.example>\r\n");
        assert_eq!(
            header_identity(&r, "P-Asserted-Identity", HeaderParams::None),
            None
        );
        assert_eq!(header_identity(&r, "From", HeaderParams::Allowed), None);
    }
}
