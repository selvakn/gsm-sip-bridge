# Phase 1 Data Model: precondition lines and the accept/decline verdict

This feature adds no persistent storage. It extends the one struct that
already represents a parsed offer (`SdpOffer`, `src/ims/sdp.rs`) and adds
small, purely-data types alongside it, following the same shape batches
4/6/7/8 already used for `MediaDirection`/`DeclinedMedia`/`rtcp`.

## SdpOffer (existing entity, extended)

| Field | Type | Role |
|---|---|---|
| `remote_rtp`, `offered`, `dtmf`, `maxptime`, `direction`, `proto`, `other_media`, `rtcp` | (existing) | Unchanged |
| **`preconditions`** | `Vec<QosDesired>` (new) | Every `a=des:qos` line from the selected audio section, in original order |
| **`offerer_curr`** | `Vec<QosStatus>` (new) | Every `a=curr:qos` line from the selected audio section, in original order — the offerer's own self-reported status, read but never asserted over (Decision 1) |

## QosStatusType (new entity — classification, not stored state)

```
enum QosStatusType { E2e, Local, Remote }
```

Parsed verbatim from an `a=des:qos`/`a=curr:qos`/`a=conf:qos` line's
status-type token. **Not inverted at parse time** — `preconditions` and
`offerer_curr` store the status type exactly as the offer wrote it
(offer-relative). Inversion (Local↔Remote, E2e unchanged) happens only
where the answer is built, so the type that means "this bridge's own
segment" is always spelled out at the call site rather than baked into a
field name — see `research.md` Decision 1.

## QosStrength (new entity)

```
enum QosStrength { Mandatory, Optional, None, Failure, Unknown }
```

Parsed from an `a=des:qos` line's strength-tag. An unrecognized token
falls back to `Unknown` rather than failing the parse — same permissive
posture already established for `proto` (specs/043 SDP-03's research.md
Decision 1).

## QosDesired (new entity — one `a=des:qos` line)

| Field | Type | Notes |
|---|---|---|
| `strength` | `QosStrength` | |
| `status_type` | `QosStatusType` | Offer-relative (unswapped) |
| `direction` | `QosDirection` | What direction(s) of the stream the desired strength applies to |

## QosStatus (new entity — one `a=curr:qos` line)

| Field | Type | Notes |
|---|---|---|
| `status_type` | `QosStatusType` | Offer-relative (unswapped) |
| `met` | `QosDirection` | How much of the resource is *currently* ready — reuses the same four wire tokens as `QosDirection` but as a status value, not a media direction (research.md Decision 6) |

## QosDirection (new entity — shared wire-token enum)

```
enum QosDirection { None, Send, Recv, SendRecv }
```

Used by both `QosDesired::direction` (what's wanted) and
`QosStatus::met` (what's currently true) — same four tokens, different
meaning depending on which line it appears on (RFC 3312 §5; see
research.md Decision 6). Kept as one type rather than two identical enums
since the wire tokens and their SDP encoding (`build_qos_line`) are
identical either way; the field name at each use site carries the
semantic difference.

## Precondition verdict (new entity — computed, not stored)

The per-INVITE decision `agent::inbound::handle_invite` makes once, right
after `sdp::parse_offer` succeeds:

```
enum PreconditionVerdict {
    /// No precondition lines, or every one is honestly answerable.
    /// Carries what to emit in the answer.
    Proceed(Vec<QosAnswerLine>),
    /// At least one `e2e`/`mandatory` line cannot be honestly confirmed.
    Decline,
}
```

`QosAnswerLine` is the small struct `build_answer_for` consumes to emit
`a=curr:qos`/`a=conf:qos` lines — one per offer precondition/status line
that produces an answer line, per the rules in `research.md` Decisions 1-2:

| Offer line | Answer line(s) produced |
|---|---|
| `a=des:qos <mandatory\|optional> remote <dir>` | `a=curr:qos local <dir>` (this bridge's segment, always immediately met for the *requested* direction — see PR #68 Greptile review: never overclaims `sendrecv` when less was asked) + `a=conf:qos local <dir>` |
| `a=des:qos <none\|failure\|unknown> remote <dir>` | `a=curr:qos local <dir>` only — strength doesn't ask for confirmation |
| `a=des:qos <mandatory> e2e <dir>` | none — `Decline` instead (Decision 2) |
| `a=des:qos <optional\|none\|failure\|unknown> e2e <dir>` | `a=curr:qos e2e <dir>` — reports what this bridge can attest to for the requested direction; never claims the far side's contribution, never claims more than was asked |
| `a=des:qos <any> local <dir>` (offerer's own segment) | none directly; if the offer also carried `a=curr:qos local <status>`, mirror it through as `a=curr:qos remote <status>` (User Story 3) — otherwise nothing emitted |
| `a=curr:qos local <status>` (offerer's self-report, no matching `a=des`) | mirrored through as `a=curr:qos remote <status>`, same as above |

## Relationship to the spec's Key Entities

| Spec term (`spec.md`) | Concrete type |
|---|---|
| Precondition line | `QosDesired` (from `a=des:qos`) / `QosStatus` (from `a=curr:qos`) |
| Precondition verdict | `PreconditionVerdict` |

## Control flow (conceptual — computed once per inbound INVITE, after `parse_offer`)

```
inbound INVITE arrives, `Require` no longer blocks on `precondition` alone
        │
        ▼
  req.body empty? ──Yes──▶ handle_offerless_invite (SDP-04, unaffected — Decision 5)
        │ No
        ▼
  offer = sdp::parse_offer(&req.body)?
        │
        ▼
  offer.proto == "RTP/AVP"? ──No──▶ decline: 488, Warning 305 (SDP-03, unchanged)
        │ Yes
        ▼
  precondition_verdict(&offer) ──Decline──▶ 580 Precondition Failure (Decision 4)
        │ Proceed(answer_lines)
        ▼
  select_codec_with(...) finds a usable codec? ──No──▶ decline: 488, Warning 304 (MT-07, unchanged)
        │ Yes
        ▼
  build_answer_for: existing m=audio/direction/declined-media lines
                  + answer_lines' a=curr/a=conf lines appended
```
