# Phase 0 Research: Honour what the far side actually offered in SDP

No `NEEDS CLARIFICATION` markers were left in the spec or Technical Context.
What Phase 0 resolves is the exact mechanism for each finding, verified
directly against current source (`gsm-sip-bridge/src/ims/sdp.rs`, 1634
lines, plus its call sites in `agent/inbound.rs`, `agent/veth.rs`,
`agent/mod.rs`).

## Decision 1: `parse_offer` stays permissive; policy decisions stay in the caller

**Decision**: `SdpOffer` grows fields for every non-selected `m=` section,
the selected audio section's direction, and its raw transport-profile
token — but `parse_offer` itself never *fails* because of any of them. It
already follows exactly this pattern for codecs today: an offer listing a
codec this client doesn't support isn't a parse error, it's just absent
from `offered`, and the caller (`handle_invite`) decides what to do about
having too few usable codecs. Extending that same shape to media sections,
direction, and transport profile keeps one policy-decision point instead
of two.

**Rationale**: `parse_offer`'s only two hard failures today are structural
(`sdp.rs:378-386`: missing `c=`, missing `m=audio`, or an empty payload-type
list) — cases where there is nothing to build a call around at all. A
second media section, an unfamiliar transport token, or a stated direction
are all *parseable* content the caller must react to differently
(decline just that policy, not necessarily the whole call setup) — mixing
that into `parse_offer`'s `Result` would collapse distinct outcomes (a
declined extra section vs. a wholesale unroutable offer) into the same
generic `Err` path, which currently produces a blunt `480 Temporarily
Unavailable` (`agent/mod.rs:1811`) with no room for SDP-03's more specific
"Warning: 305" decline the spec calls for.

**Alternatives considered**:
- Fail `parse_offer` outright on an unrecognized transport token — rejected:
  collapses into the existing generic-error path (`480`), losing the
  distinct, spec-mandated response the codec-mismatch case already gets
  its own status/Warning pair for (`488`/Warning 304). SDP-03 asks for
  exactly that same treatment (Warning 305), which requires the decision
  to be made in `handle_invite`, not buried in a `Result::Err`.
- A separate `SdpOffer::validate()` pass after parsing — rejected: splits
  one offer's semantics across two functions for no benefit; the fields
  are already just data the caller inspects, same as `offered`/`dtmf`
  today.

## Decision 2: track every `m=` section in offer order, decline the rest

**Decision**: `parse_offer` selects the *first* `m=audio` section for real
negotiation (fixing the existing last-wins overwrite —
`sdp.rs:334-342` currently lets a second `m=audio` line's `rtp_port`/
`listed_pts` silently replace the first's). Every other `m=` section —
another audio line, video, text, application, anything — is captured as a
lightweight `DeclinedMedia { kind, proto, fmts, before_audio }` entry, in
original order, and `build_answer_for` emits one `m=<kind> 0 <proto>
<fmts>` line per entry (RFC 3264 §6: a declined stream is marked with port
`0`) in the same relative position.

**Rationale**: RFC 3264 §6 requires the answer to have the same number of
media descriptions as the offer, in the same order — omitting a section
entirely (today's behavior, confirmed via the existing
`PJSIP_REAL_VETH_OFFER` fixture's `m=text` section, which currently has
zero effect on the answer) is not a valid answer to that offer, not merely
an incomplete one. No `c=` line is needed per declined section — the
existing session-level `c=` line already covers it per RFC 4566 §5.7, and
a declined stream carries no media regardless.

**Alternatives considered**:
- Actually relay a second audio stream, or video/text — rejected outright
  per the spec's Assumptions: this bridge is a single-audio-stream relay
  by design (`sdp.rs`'s own header comment: "not a general-purpose SDP
  library"), and building real multi-stream relay support is a materially
  larger, currently unjustified feature (no carrier here has sent more
  than one media section).
- Track sections as a fully generic ordered list (offer-shaped struct
  mirroring every section uniformly) rather than "one selected + N
  declined" — rejected: over-general for a bridge that will only ever
  negotiate the one audio section; the asymmetric shape says directly
  what the code does.

## Decision 3: direction is mirrored in the answer, never gates the relay

**Decision**: `SdpOffer` gains `direction: MediaDirection` (`SendRecv` /
`SendOnly` / `RecvOnly` / `Inactive`), parsed from the selected audio
section's own `a=sendonly`/`recvonly`/`inactive`/`sendrecv` line
(`sdp.rs:352-382`'s attribute loop currently recognizes none of these).
`build_answer_for` replaces its hardcoded `a=sendrecv\r\n`
(`sdp.rs:721`) with the RFC 3264 §6.1 mirror: `SendOnly`→`RecvOnly`,
`RecvOnly`→`SendOnly`, `Inactive`→`Inactive`, `SendRecv`/absent→`SendRecv`.
The RTP relay's actual send/receive behavior is unchanged — this is a
signaling-correctness fix, not a media-suppression feature.

**Rationale**: The mirror mapping is RFC 3264 §6.1's literal requirement
for what an answer's direction attribute must say relative to the offer's;
nothing here requires the relay to *act* on it, and no carrier this bridge
has seen has sent anything but two-way audio on an initial offer (the same
observed-absence rationale already accepted for MT-02's mid-call
renegotiation scope, `docs/plans/mt-conformance-findings.md`).

**Alternatives considered**:
- Also gate the relay (stop forwarding/accepting RTP in the declared
  direction) — rejected: `agent/veth.rs`'s pass-through relay and
  `transcode.rs`'s transcoding relay have no per-direction suppression
  today, and building it is a separate, materially larger feature (this is
  also scoped as its own possible future finding, not part of SDP-02).
  Doing the signaling fix alone is strictly correct and doesn't misstate
  the relay's real behavior any worse than today — a caller reading
  `a=recvonly` off our answer and choosing not to send is the caller's own
  business, exactly as intended.

## Decision 4: transport-profile mismatch gets its own `488`/Warning 305, decided in `handle_invite`

**Decision**: `SdpOffer` gains `proto: String` — the audio section's raw
`m=` transport token (`sdp.rs:341`'s `fields.skip(1)` currently discards
this). `handle_invite` (`agent/inbound.rs`) checks it immediately after
`parse_offer` succeeds, before the existing codec-selection check
(`inbound.rs:153-155`): if it isn't the literal `"RTP/AVP"` this bridge
implements, decline with a new `build_488_incompatible_transport`,
carrying `Warning: 305 ... "incompatible network protocol used"` (RFC
3261 §20.43's table entry for exactly this situation) — visibly distinct
from the existing codec-mismatch `488`/Warning 304
(`build_488_not_acceptable`), per the spec's explicit requirement that the
two declines be distinguishable.

**Rationale**: `RTP/SAVP` (SRTP) or a garbage token describe a stream this
bridge structurally cannot service (no SRTP support anywhere in the
codebase) — accepting the offer's codecs while ignoring what it said about
the transport would mean answering a protocol the offer never actually
proposed. Warning 305 is RFC 3261's own table entry for "a certain
transport protocol was included ... but the client does not support that
transport protocol" — a precise fit, and the same "one new Warning-text
builder per genuinely distinct decline reason" pattern already used for
304 (MT-07) and the `Unsupported`/`Accept` headers on `420`/`415`.

**Alternatives considered**:
- Reuse `build_488_not_acceptable` verbatim (Warning 304, media type) for
  this too — rejected: the spec explicitly requires the transport decline
  to be distinguishable from the codec decline, and 304 is documented as
  meaning "media type," not "transport protocol" — reusing it would be
  the same kind of imprecise-but-plausible response this whole review
  batch exists to eliminate.
- `606 Not Acceptable` (a different status code entirely, RFC 3261
  §21.6.6, session-characteristics-not-acceptable) — rejected: `488` at
  the same status-code family as the existing codec decline, differing
  only in `Warning:` text, keeps the pattern consistent with what's
  already established for MT-07 rather than introducing a second decline
  status code for a conceptually similar situation.
- Fail inside `parse_offer` (see Decision 1) — rejected there already.

## Decision 5: MT-05 needs a confirming test, not new behavior

**Decision**: Add one test proving an inbound INVITE's `Session-Expires`
header (if present) is never echoed in the `200 OK`, and mark MT-05 `[x]`
in the tracking doc with a "confirmed resolved, no new code" writeup
rather than any change to `agent/inbound.rs`'s response headers.

**Rationale**: `SUPPORTED_EXTENSIONS` is empty (`agent/mod.rs:378`, since
MT-10), so the inbound side no longer advertises `timer` support at all —
the finding's premise ("advertised, never honoured") no longer holds
post-MT-10. RFC 4028 §9 explicitly permits a UAS to simply omit
`Session-Expires` from its response when it doesn't want the extension;
that is exactly today's behavior and is fully spec-legal, not a defect.
Building real session-timer support would require either accepting a
refresh burden this bridge structurally can't fulfill (it never sends its
own re-INVITE) or surviving a refresh re-INVITE from the far end, which
collides with batch 3's now-unconditional `488` decline of every
re-INVITE (`specs/042-dialog-transaction-identity`) — reopening exactly
the renegotiation scope MT-02 already ruled out, for a scenario no carrier
here has ever sent.

**Alternatives considered**:
- Echo `Session-Expires: <n>;refresher=uac` to force the far end to own
  any refresh — rejected: non-zero risk (a carrier enforcing a short
  interval could tear the call down after a refresh re-INVITE gets
  declined by batch 3's blanket policy) for a scenario with zero observed
  evidence, against a zero-risk, fully spec-legal alternative (do nothing,
  as today). Matches this codebase's consistent preference for the
  smaller-blast-radius option when there's no live evidence forcing the
  larger one.
- Add `timer` to `SUPPORTED_EXTENSIONS` — rejected: would mean claiming a
  capability (`Require: timer` no longer refused) whose only real behavior
  is "accept the header and then do nothing with it," which is the same
  capability-truthfulness problem MT-10 already eliminated elsewhere.
