# Data model: Caller identity from `tel:` URIs

All types are crate-internal (`pub(crate)`) and live in
`gsm-sip-bridge/src/ims/identity.rs`. Nothing is persisted, and no stored
record changes shape. CDRs and alerts still receive a `String` caller.

## HeaderParams

Says whether a header defines header parameters. This decides how an
unbracketed `addr-spec` ends (research R2).

| Variant | Headers | Unbracketed `;…` belongs to |
|---|---|---|
| `Allowed` | `From` (and `To`, `Contact` if ever needed) | the header, so it is cut |
| `None` | `P-Asserted-Identity` | the URI, so it is kept |

## NameAddr<'a>

One parsed header value.

| Field | Type | Rule |
|---|---|---|
| `display` | `Option<String>` | Unescaped quoted-string or trimmed token run. `None` if absent or empty, if the quote is unterminated, or if it contains CR/LF (FR-012). |
| `uri` | `&'a str` | Text inside `<…>`, or the bare `addr-spec` cut per `HeaderParams`. Must contain `scheme:`, otherwise the whole value is `None`. |

Only scheme-bearing values count. `"Anonymous"` alone parses to `None`.

## Identity

The caller as taken from one header (FR-009 to FR-011).

| Field | Type | Rule |
|---|---|---|
| `number` | `String` | From `uri_number`: the `tel` value first, then the first `sip`/`sips` value that yields one. |
| `display` | `Option<String>` | The first non-empty `display` among the **same header's** values. |

An `Identity` exists only if `number` resolved. Choosing the header:

```text
header_identity(PAI)  ──Some──▶ caller = PAI.number,  name = PAI.display
        │ None
        ▼
header_identity(From) ──Some──▶ caller = From.number, name = From.display
        │ None
        ▼
caller = "unknown", name = None   (+ FR-015 diagnostic at the request's single log point)
```

## uri_number(uri) → Option<String>

| Scheme (case-insensitive) | Raw part | Phone number when |
|---|---|---|
| `tel` | text before the first `;` | always |
| `sip`, `sips` | text before the first `@`, minus `:password`, cut at `;`. No `@` means `None`. | `user=phone` among the URI params, or the user starts with `+` |
| anything else | – | `None` |

Then apply, in order:

1. Percent-decode. An invalid escape means `None`.
2. If it's a phone number, remove `- . ( )`.
3. Allow-list `[A-Za-z0-9+*#\-._~]`. A non-empty result is returned,
   anything else means `None`.

## Delivery-report address

`header_uri(req, name)` returns `Option<String>`, the whole URI.

- Collect `NameAddr`s from every line of `name`, using `HeaderParams` for
  that header.
- Return the first `sip`/`sips` URI, otherwise the first URI.

The callers stay the same: `header_uri(PAI).or_else(|| header_uri(From))`.
