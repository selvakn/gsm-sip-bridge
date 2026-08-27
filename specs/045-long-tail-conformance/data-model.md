# Phase 1 Data Model: The long tail

No persistent storage. Small extensions to existing types, plus two new
small enums.

## `DecodedRp` (existing entity, extended — `src/ims/sms_pdu.rs`)

```rust
pub enum DecodedRp {
    Message(DecodedSms),           // unchanged
    Ack { rp_mr: u8 },              // unchanged
    Error { rp_mr: u8, cause: Option<u8> },  // unchanged
    /// New (SMS-02): the RP-DATA envelope was fine; its own TP-MTI says
    /// the TPDU inside isn't SMS-DELIVER. Recognized, nothing to relay —
    /// caller sends a plain 200 OK, same as Ack/Error.
    UnsupportedTpdu { rp_mr: u8, kind: TpduMessageType },
    /// New (SMS-03): the TPDU claimed to be SMS-DELIVER but its bytes
    /// don't parse as one. Caller sends an RP-ERROR instead of relaying
    /// req.body as if it were text.
    Undecodable { rp_mr: u8 },
}
```

## `TpduMessageType` (new — `src/ims/sms_pdu.rs`)

```rust
pub enum TpduMessageType { Deliver, SubmitReport, StatusReport, Reserved }
```

Derived from a TPDU's first octet's low 2 bits (TS 23.040 §9.2.3.1, SC→MS
direction). Used only to label `UnsupportedTpdu` for diagnostics — no
downstream behavior branches on which non-Deliver type it was.

## `SubscribeParts` (existing entity, extended — `src/ims/session.rs`)

| Field | Type | Role |
|---|---|---|
| `access_network_info` | `&'a str` (new) | Echoed into `P-Access-Network-Info` in place of the hardcoded `"3GPP-WLAN"` literal |

## `UAS_EXTRA_HEADERS` (existing const, becomes per-call — `src/ims/agent/inbound.rs`)

Was `const UAS_EXTRA_HEADERS: &[(&str, &str)] = &[("Allow", ALLOW)]`.
Becomes a small `Vec<(&str, String)>` (or equivalent) built once per
`handle_invite` call, `Allow` plus `P-Access-Network-Info` sourced from
`ctx`/`session`'s already-resolved access-network value.

## New functions (no new types)

| Function | File | Role |
|---|---|---|
| `annotate_via_received_rport(message: &str, peer: SocketAddr) -> String` | `sip_client.rs` | MT-13: adds `received=`/fills `rport=` on a response's top `Via`, no-op on a request |
| `SipSink::peer_addr(&self) -> Option<SocketAddr>` | `sip_client.rs` | MT-13: exposes the real peer address already known to `SinkInner` (UDP) or the socket (`TcpStream::peer_addr`, TCP) |
| `build_rp_error(rp_mr: u8, cause: Option<u8>) -> Vec<u8>` | `sms_pdu.rs` | SMS-03: RP-ERROR body, mirrors `build_rp_ack`'s existing shape |
| quote-aware comma-split helper | `modules/worker.rs` | CS-04: replaces `line.split(',')` in `parse_sms_response` |

## Relationship to the spec's Key Entities

| Spec term (`spec.md`) | Concrete type |
|---|---|
| Asserted identity | `session::extract_caller`'s new `P-Asserted-Identity`-first lookup |
| TPDU type | `TpduMessageType` |
| RP-ERROR | `sms_pdu::build_rp_error`, `DecodedRp::Undecodable` |

## Decode flow (SMS-02/SMS-03, `decode_vnd_3gpp_sms`)

```
RP-DATA envelope parses
        │
        ▼
  TP-MTI (first octet & 0x03) == SMS-DELIVER? ──No──▶ DecodedRp::UnsupportedTpdu { rp_mr, kind }
        │ Yes
        ▼
  SmsDeliverTpdu::parse succeeds? ──No──▶ DecodedRp::Undecodable { rp_mr }
        │ Yes
        ▼
  DecodedRp::Message(decoded)
```
