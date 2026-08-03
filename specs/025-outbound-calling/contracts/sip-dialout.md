# Contract: the on-the-wire SIP behavior toward PBX and phones

**Feature**: 025-outbound-calling

**Note (2026-08-03)**: "that account's UAS handler" below assumes the
pjsua-safe UAS additions in research.md R-007 (`Call::from_id`,
`Call::answer`, `on_incoming_call`) — pjsua-safe has no such handler today.
That work is a prerequisite for this contract to be implementable at all.

## PBX path

No new listener (research.md R-002). The PBX sends an `INVITE` to the
contact the bridge already registered via its existing trunk `Account`
(`sip::SipBridge`). With `[outbound].enabled = true`, that account's UAS
handler (R-007) accepts it (rather than the pre-feature behavior, which
never receives one because nothing on the PBX side has reason to send it);
with the flag `false` the account behaves exactly as it does today — this
feature adds a branch, it does not change what the trunk registration
already means.

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
            Contact: sip:{destination}@{listen_addr}:{sip.local_port}
```

`{destination}` is the *dialed number* — the user part of the original
INVITE's own Request-URI (`sip::server::uri_user`), not the phone's AOR.
**Revised 2026-08-03** (specs/025-outbound-calling review): the Contact
used to carry `{aor}`, on the assumption the phone would preserve the
original destination into the To header of its retry INVITE regardless of
what the Contact itself said. RFC 3261 §8.1.3.4 only says a UAC *MAY* copy
a 3xx's Contact into its retry's Request-URI, and nothing requires it to
otherwise preserve the original request's To value across the retry — a
handset that just follows the redirect as given would send its own AOR as
the destination. Putting the real destination in the Contact directly
means this works regardless of what the retry's To header turns out to be.

The phone re-INVITEs that Contact. That second INVITE lands on
`Account::local` (spec 024) inside the daemon's real pjsua stack, which
accepts it as UAS with real SDP/media — this is where `OutboundCallRequest`
is actually constructed and line selection begins. `Call::request_destination`
extracts the destination from *that* request's own To header (see its doc
comment) — for a well-behaved handset this now agrees with what the
Contact already said, rather than being the only place the destination
survives.

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
| No answer within the ring window | `480 Temporarily Unavailable`, reported as `OutboundAttemptOutcome::Unanswered` (SC-005) — distinct from a carrier rejection |

**Status, 2026-08-03 (specs/025-outbound-calling review)**: this table was
entirely aspirational for the VoWiFi/VoLTE path until this pass — the phone
leg was answered `200` only after the carrier's own `200`, with no `180`
relayed in between (silence for up to `OUTBOUND_INVITE_TIMEOUT +
OUTBOUND_RING_TIMEOUT`, 75s, then a sudden answer), and every carrier
rejection collapsed to a blanket `503` regardless of the carrier's actual
status. Now implemented via:

- `ControlMessage::CallRinging` (Agent A → Agent B, `contracts/agent-outbound-protocol.md`), sent once per call on the carrier's `180`; Agent B relays it as `call.answer(180)` on the phone/PBX leg.
- `vowifi::mod::carrier_status_from_reason`/`outbound_outcome_for_committed_failure`, which read the carrier's real status back out of `CallFailed.reason` (`ims::agent::fail`'s non-2xx branch formats it as `"{status} {reason}"`) instead of always answering `503`, and distinguish a genuine no-answer (`ims::agent`'s `reason::CARRIER_TIMEOUT` marker, or an explicit carrier `480`) as `Unanswered` from every other rejection.

The CS/AT-dial path's own equivalent of this table (T019) is still
unimplemented and separately, honestly flagged in `tasks.md` — this fix is
VoWiFi/VoLTE-only.

## Teardown

Unchanged from existing call handling (FR-013): either leg hanging up tears
down the other, using the same code path an inbound call's teardown already
uses — this feature does not introduce a second teardown implementation.
