# Contract: the on-the-wire SIP behavior toward PBX and phones

**Feature**: 025-outbound-calling

## PBX path

No new listener (research.md R-002). The PBX sends an `INVITE` to the
contact the bridge already registered via its existing trunk `Account`
(`sip::SipBridge`). With `[outbound].enabled = true`, that account's UAS
handler accepts it (rather than the pre-feature behavior, which never
receives one because nothing on the PBX side has reason to send it); with
the flag `false` the account behaves exactly as it does today — this feature
adds a branch, it does not change what the trunk registration already means.

Request-URI user part → `OutboundCallRequest.destination` verbatim (FR-010).

## SIP server mode path

The registrar's `INVITE` handling (`sip::server::mod::handle_datagram`)
changes from:

```text
"INVITE" => refuse with 403 Forbidden   # spec 024 FR-020, [outbound] disabled
```

to, when `[outbound].enabled = true` and the request is from a **currently
registered** account (any account, not only `ring_aor` — clarification
2026-08-02):

```text
"INVITE" => 302 Moved Temporarily
            Contact: sip:{aor}@{listen_addr}:{sip.local_port}
```

The phone re-INVITEs that Contact. That second INVITE lands on
`Account::local` (spec 024) inside the daemon's real pjsua stack, which
accepts it as UAS with real SDP/media — this is where `OutboundCallRequest`
is actually constructed and line selection begins.

An INVITE from an address with **no live binding** (never registered, or
lapsed) is still refused — `403 Forbidden`, matching today's refusal for any
unauthenticated attempt. Redirect is only ever offered to an already-
authenticated phone, so this feature adds no new unauthenticated attack
surface: everything reaching `OutboundCallRequest` construction already
passed the registrar's existing digest authentication (spec 024 FR-008–010).

With `[outbound].enabled = false` (or absent), the branch is unchanged —
`403 Forbidden` for every INVITE, byte-for-byte as spec 024 shipped it
(FR-017).

## Call progress relay (FR-012)

Once `OutboundCallRequest` is placed, mobile-network progress is relayed
using the same signalling the bridge already emits for other call outcomes:

| Mobile network event | SIP response to PBX/phone |
|---|---|
| Ringing | `180 Ringing` |
| Busy | `486 Busy Here` |
| Rejected / barred / unreachable | `503 Service Unavailable` (matches existing "PBX unreachable" handling) |
| Answered | `200 OK` with SDP, media flows |
| No line was idle (FR-009) | `503 Service Unavailable` sent immediately, before any ringing indication — distinguishable in logs/metrics (FR-016) from a network-side `503` |

## Teardown

Unchanged from existing call handling (FR-013): either leg hanging up tears
down the other, using the same code path an inbound call's teardown already
uses — this feature does not introduce a second teardown implementation.
