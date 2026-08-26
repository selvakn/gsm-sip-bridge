# Phase 0 Research: Match in-dialog SIP requests to the call they name

No `NEEDS CLARIFICATION` markers were left in the spec or in Technical
Context — the codebase, language, and test tooling are all fixed by the
existing project (single Rust crate, no new dependencies). What Phase 0
actually resolves here is the *mechanism* behind each functional requirement,
each verified directly against the current source (two Explore passes plus a
targeted re-read of the exact call sites) rather than assumed.

## Decision 1: Identity check, not a transaction table

**Decision**: Add a Call-ID (and, for INVITE, CSeq) comparison at each of the
three dispatch points that need it (INVITE, BYE, CANCEL), checked against the
single `active_call` slot `LoopState` already holds. No transaction table, no
per-transaction timers, no support for more than one transaction/dialog per
line.

**Rationale**: `LoopState` (`src/ims/agent/mod.rs:1207-1269`) holds exactly
one call slot each for inbound (`active_call: Option<ActiveCall>`) and
outbound (`origination: Option<PendingOrigination>`) — confirmed by reading
the struct directly. This is deliberate: multi-modem parity runs one
`LoopState`/process per physical line, and admission is already governed by
`Admission::for_current` refusing a second call outright
(`src/ims/lifecycle.rs:238-252`). There is no evidence anywhere in the
codebase of concurrent transactions needing to be tracked on one line.
Constitution Principle V (Simplicity & Refactorability — YAGNI) directly
rules out building generic multi-transaction machinery against a
architecture that only ever has one.

**Alternatives considered**:
- A generic RFC 3261 §17 transaction-table engine (arbitrary transaction
  count, T1/T2/Timer A–K) — rejected: unjustified complexity for a
  confirmed one-call-per-line bridge; nothing in the codebase or its
  multi-modem design implies more than one dialog per line will ever exist.

## Decision 2: CSeq equality is sufficient to detect a retransmission

**Decision**: A repeated `INVITE` naming the active call is detected by
comparing its raw `CSeq` header value against the `CSeq` of the `INVITE` the
system already answered (or is already ringing on), not by parsing/comparing
the `Via` branch parameter.

**Rationale**: RFC 3261 §12.2.2 requires every subsequent in-dialog request
to carry a strictly higher CSeq than any request before it in that dialog.
An exact CSeq match on the same Call-ID can therefore only be a
retransmission of that exact request — never a legitimate new request. This
holds regardless of transport-level branch/duplication behavior, so no
branch parsing is needed for this determination. `SipRequest`
(`src/ims/sip_client.rs:223-347`) already exposes `header("CSeq")`; no new
parsing helper is needed for this specific check (a full CSeq-number parser
already exists for a different purpose, `ims::agent::ping::parse_cseq_number`,
but string equality is sufficient and simpler here — Simplicity gate).

**Alternatives considered**:
- Via-branch-based transaction identity (the canonical RFC 3261
  §17.1.3/§17.2.3 "magic cookie" mechanism) — more textbook-general, but adds
  parsing complexity with no behavioral difference in a bridge that never has
  more than one transaction in flight per role. Rejected under Simplicity.

## Decision 3: CANCEL after a final answer gets an explicit 200, not silence or a blanket 481

**Decision**: A `CANCEL` naming the call currently active (already given a
final response) is answered `200 OK` on that call's own `To` tag. A `CANCEL`
naming anything else keeps falling through to the existing
`unserved_method_response` catch-all, which already answers `481` — this
path is unchanged.

**Rationale**: RFC 3261 §9.2 explicitly requires a `200 OK` to the `CANCEL`
request itself, on the same `To` tag as the original request's response,
even when the `CANCEL` arrives too late to affect the outcome. Verified that
today `dispatch_loop` (`src/ims/agent/mod.rs:1330-1395`) has no `CANCEL` arm
at all — every `CANCEL` reaching dispatch falls to
`unserved_method_response` (`mod.rs:427-447`), which hardcodes `481`
regardless of whether the named call is real. That behavior is only correct
for a `CANCEL` naming something that genuinely doesn't exist; it's wrong for
one naming the call that's active or was just answered.

**Alternatives considered**:
- Leave the current unconditional `481` — rejected, contradicts the RFC's
  explicit MUST.
- Track full CANCEL/INVITE transaction pairing generically — rejected under
  Decision 1's reasoning; Call-ID equality against the one active call is
  sufficient here.

## Decision 4: Re-INVITE declines with 488, reusing the existing MT-07 precedent

**Decision**: An `INVITE` naming the active call that is *not* a retransmission
(per Decision 2) is declined with `488 Not Acceptable Here`, using the
already-existing `build_488_not_acceptable` (`src/ims/sip_client.rs:503`,
landed for MT-07's codec-mismatch decline).

**Rationale**: `486 Busy Here` tells the network the line is occupied, which
drives call-forward-on-busy treatment on the caller's side for the wrong
reason — the line is not busy, this bridge simply cannot renegotiate a call
already in progress. `488` with the existing `Warning: 304` pattern already
established for exactly this "can't do it, not busy" distinction (MT-07)
extends cleanly to this case rather than inventing a new response shape.

**Alternatives considered**:
- Silently answer with the unchanged, already-negotiated SDP as if accepting
  the re-INVITE — rejected: misrepresents acceptance of a change that might
  be substantive (hold, codec change), which is worse than an honest decline.
- `500 Server Internal Error` — rejected: less precise than `488`, and `500`
  is reserved for the bridge's own failures, not a deliberate scope
  boundary.

## Decision 5: An unmatched or missing-context BYE is refused, never silently accepted

**Decision**: A `BYE` not naming the active call — including one arriving
when no call is active at all — is refused `481 Call/Transaction Does Not
Exist`, using the same builder pattern as the CANCEL/other-unmatched cases.

**Rationale**: Verified `handle_carrier_bye` (`mod.rs:1699-1712`) currently
does `self.active_call.take()` unconditionally, and answers a `BYE` with **no**
active call `200 OK` — falsely implying a dialog existed. No test in the
codebase pins that specific `200 OK`-with-no-call behavior, so correcting it
alongside the primary Call-ID check is not a breaking contract change.

**Alternatives considered**:
- Silently drop an unmatched `BYE` (no response at all) — rejected: leaves a
  compliant peer retransmitting per its own transaction timer, which this
  codebase's own `unserved_method_response` doc comment already argues
  against for exactly this reason ("a request left unanswered ... draws its
  own conclusion ... a worse outcome").
