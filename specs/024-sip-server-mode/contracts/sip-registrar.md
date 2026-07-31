# Contract: SIP registrar behaviour toward IP phones

**Feature**: 024-sip-server-mode

This is the on-the-wire contract. Every row below is asserted by a test in
`gsm-sip-bridge/tests/test_sip_server_registrar.rs`, driven over a real loopback
UDP socket against the real registrar — no mocks.

Transport: **UDP only** in this version, on `[sip_server].listen_addr:listen_port`.

---

## 1. REGISTER

### 1.1 Unauthenticated request → `401 Unauthorized`

Any REGISTER without an `Authorization` header is challenged. No binding is
created.

```
WWW-Authenticate: Digest realm="<realm>", nonce="<32 hex chars>", qop="auth", algorithm=MD5
```

The nonce is recorded as live for `nonce_lifetime_sec`.

### 1.2 Correct credentials → `200 OK`

Accepted when the `Authorization` header's `response` matches, computed as:

- `HA1 = MD5(username ":" realm ":" password)`
- `HA2 = MD5("REGISTER" ":" uri)`, where `uri` is the client's own `uri`
  parameter verbatim
- with `qop=auth`: `response = MD5(HA1 ":" nonce ":" nc ":" cnonce ":" qop ":" HA2)`
- without `qop`: `response = MD5(HA1 ":" nonce ":" HA2)`

**Both forms are accepted** — handsets in the field send both.

The `200 OK` echoes the registered `Contact` with the granted lifetime:

```
Contact: <sip:1001@192.168.1.50:5060>;expires=3600
```

The nonce is consumed. A binding is created or replaced.

### 1.3 Wrong password → `401 Unauthorized`, no `stale`

Byte-identical to §1.4. No binding is created; any existing binding is left
untouched.

### 1.4 Unknown username → `401 Unauthorized`, no `stale`

**Byte-identical to §1.3** — the response must not reveal whether the account
exists (FR-009). Only the metric label distinguishes the two.

### 1.5 Expired or unknown nonce → `401 Unauthorized` with `stale=true`

```
WWW-Authenticate: Digest realm="<realm>", nonce="<fresh nonce>", qop="auth", algorithm=MD5, stale=true
```

`stale=true` tells the handset to retry silently rather than prompting a human
for a password. Applies when the nonce has aged past `nonce_lifetime_sec`, was
already consumed, or was evicted by the table cap.

### 1.6 Replayed nonce-count → `401 Unauthorized`, no `stale`

Under `qop=auth`, `nc` must strictly increase per nonce. Without `qop`, the
nonce is single-use, so a replay lands in §1.5.

### 1.7 Unsupported algorithm → `401 Unauthorized`

`MD5-sess` and `SHA-256` are rejected; absent and `MD5` are accepted.

### 1.8 `Expires` below `min_expires` → `423 Interval Too Brief`

```
Min-Expires: <min_expires>
```

No binding is created.

### 1.9 `Expires` above `max_expires` → `200 OK`, clamped

The binding uses `max_expires`, and the `Contact` in the response reports that
granted value — never the requested one.

### 1.10 `Expires: 0` → `200 OK`, de-registered

Accepted as `Contact: *` with `Expires: 0`, or a specific `Contact` with
`Expires: 0` (header or `;expires=0` parameter). The binding is removed and the
`200 OK` carries **no** `Contact`. Still requires valid credentials.

### 1.11 Retransmission → `200 OK`, unchanged

A REGISTER whose `Call-ID` matches the stored binding and whose `CSeq` is less
than or equal to the stored one is a retransmission or a reorder. The response
reports the **existing** binding and the expiry is **not** extended.

> Deviation from RFC 3261 §10.3 step 6, which prefers `500 Server Error` for a
> lower `CSeq` on the same `Call-ID`. A `200 OK` reporting current state is
> friendlier to handsets and equally safe here, since the binding is not
> modified. Recorded deliberately.

### 1.12 Malformed request → `400 Bad Request`

Unparseable request line, missing `Call-ID`/`CSeq`/`From`/`To`/`Via`, or a
`Contact` that is neither `*` nor a parseable URI.

---

## 2. Other methods

| Method | Response | Notes |
|---|---|---|
| `OPTIONS` | `200 OK` + `Allow: INVITE, ACK, BYE, CANCEL, OPTIONS, REGISTER` | Handset keepalive. Must be answered — unanswered, handsets mark the server dead and drop their binding. |
| `INVITE` | `403 Forbidden` + WARN naming the caller | Phone-originated dialling is out of scope (FR-020). An explicit refusal beats a 32-second retransmit and a timeout on the handset's screen. |
| `SUBSCRIBE` | `489 Bad Event` | Message-waiting and busy-lamp subscriptions are unsupported. |
| anything else | `405 Method Not Allowed` + `Allow:` | RFC-correct default. |

No request is ever silently dropped (FR-015).

---

## 3. Response construction, all methods

Every response is built through `ims::sip_client::build_uas_response_with_headers`,
which guarantees:

- **all** `Via` headers echoed, in order — a request arriving through more than
  one hop must have its full `Via` stack returned
- `From`, `Call-ID` and `CSeq` copied verbatim
- a `To` tag added only when the request's `To` had none (RFC 3261 §8.2.6.2)
- `Content-Length: 0` — no response in this contract carries a body

---

## 4. Outbound INVITE toward a registered phone

Placed by the existing pjsua call path, **not** by the registrar, from
`[sip].local_port`.

| Property | Value |
|---|---|
| Request-URI | the binding's `contact_uri`, **verbatim** |
| `From` | `"<display_name>" <sip:{ring_aor}@{listen_addr}:{listen_port}>` |
| `P-Asserted-Identity` | `"<did>" <tel:<did>>` — unchanged from the PBX case |
| `X-GSM-Caller-ID` | the caller's number — unchanged from the PBX case |
| Media, codecs, teardown | unchanged from the PBX case (FR-021) |

The phone therefore sees an INVITE sourced from a port other than the one it
registered to. This is RFC-correct — delivery uses the phone's own `Contact`,
responses follow `Via`+`rport`, and ACK/BYE follow our `Contact`. See
research.md R-002 for the one handset setting that interacts with it.

**When no live binding exists for `ring_aor`**, no INVITE is placed: the carrier
call is left to ring out, the cause is logged naming `ring_aor`, and
`ring_target_missing_total` is incremented (FR-018).
