# Phase 1 Data Model: Match in-dialog SIP requests to the call they name

This feature adds no persistent storage and no new top-level entity — it adds
identity fields/checks to the one call-state entity that already exists.
Types are named as they will actually appear in `src/ims/agent/call.rs` and
`src/ims/agent/mod.rs` (verified against current struct/function definitions).

## Active call (existing entity, extended)

`ActiveCall` (`src/ims/agent/call.rs:58-86`) — the single call, if any,
occupying a line. Existing identity fields already present and reused as the
match key for every check in this feature:

| Field | Type | Role in this feature |
|---|---|---|
| `call_id` | `String` | The comparison key for every in-dialog request (BYE, CANCEL, ACK, re-INVITE) |
| `to_tag` | `String` | Used to answer a late `CANCEL` on the same dialog identity the original response established |

**New field**:

| Field | Type | Role |
|---|---|---|
| `answered_invite` | `Option<CachedInviteAnswer>` | `Some` for a call this side answered as UAS (has a real answer to resend on retransmission); `None` for a call this side placed itself (UAC role) — an outbound-placed call has no inbound INVITE of its own to have answered |

## CachedInviteAnswer (new entity)

What's needed to resend the exact `200 OK` that answered a call's INVITE, if
a retransmission of that INVITE arrives later (Decision 2/Decision 3 in
`research.md`).

| Field | Type | Notes |
|---|---|---|
| `invite_cseq` | `String` | Raw `CSeq` header value of the INVITE that was answered (e.g. `"1 INVITE"`) — RFC 3261 §12.2.2 guarantees an exact match can only be a retransmission |
| `contact` | `String` | Echoed back verbatim on a resent `200 OK` |
| `answer_sdp` | `String` | Echoed back verbatim on a resent `200 OK` |

Constructed once, at the same point `ActiveCall` itself is constructed after
a successful answer (`src/ims/agent/inbound.rs:387`, where `contact` and
`answer_sdp` are already owned locals — this is a move, not a new
allocation). The second `ActiveCall` construction site
(`src/ims/agent/origination.rs:1536`, the UAC/outbound-answered case) sets
this field to `None`.

## InDialogInvite (new entity — classification, not stored state)

A pure classification of an inbound INVITE that names the active call's
Call-ID, computed fresh from the request and the active call's
`answered_invite` — never itself persisted.

```
enum InDialogInvite {
    RetransmittedOriginal,  // same Call-ID, CSeq == answered_invite.invite_cseq
    ReInvite,               // same Call-ID, anything else (including answered_invite: None)
}
```

State transition (conceptual, not a stored state machine — computed once per
inbound INVITE):

```
inbound INVITE arrives
        │
        ▼
  Call-ID matches active_call? ──No──▶ fall through to existing busy/new-call handling (unchanged)
        │ Yes
        ▼
  answered_invite is Some AND CSeq matches it? ──Yes──▶ RetransmittedOriginal → resend cached 200 OK
        │ No
        ▼
      ReInvite → decline 488 (Decision 4)
```

## Relationship to the spec's Key Entities

| Spec term (`spec.md`) | Concrete type |
|---|---|
| Active call | `ActiveCall` |
| In-dialog request | Any `SipRequest` where `method` is `BYE`/`CANCEL`/`ACK`/`INVITE` and its `Call-ID` header is compared against `ActiveCall.call_id` |
| Repeated offer | `InDialogInvite::RetransmittedOriginal` |
