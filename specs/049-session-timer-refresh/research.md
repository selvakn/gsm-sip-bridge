# Phase 0 Research: RFC 4028 session-timer refresh (outbound/UAC leg)

`docs/todo.md`'s longest-open item, picked up now per the user's explicit
request rather than waiting for either of its two stated triggers. Two
Explore passes (current `origination.rs`/`agent/mod.rs`/`call.rs` response
and teardown code) plus the actual RFC 4028 text (fetched directly —
`rfc-editor.org/rfc/rfc4028.txt` — not recalled from memory, per this
project's own SMS-07/specs/048 precedent for protocol text) resolved this
into a bounded feature.

## Decision 1: Refresh transport is `UPDATE` only, both directions — no re-INVITE-based refresh is built

**Decision**: This bridge's own outgoing refreshes (User Story 1) and its
acceptance of the carrier's refreshes (User Story 2) both use `UPDATE`
exclusively. No re-INVITE-based session refresh is sent or accepted by this
feature.

**Rationale**: RFC 4028 §7.4 (verbatim): *"If a UAC knows that its peer
supports the UPDATE method, it is RECOMMENDED that UPDATE be used instead
of a re-INVITE... It is RECOMMENDED that the UPDATE request not contain an
offer... but a re-INVITE SHOULD contain one."* UPDATE is both the
RFC-preferred transport and the one that avoids touching this bridge's
SDP offer/answer machinery at all (a bodyless UPDATE, exactly as
recommended) — re-INVITE-based refresh would instead have to reuse the
same offer/answer code path real call setup uses, for a no-op renegotiation,
which is real added risk (accidentally changing media) for no benefit here.

The RFC also says a UAC should only use UPDATE once it "knows" the peer
supports it (having seen `Allow: UPDATE`). This bridge does not currently
capture or check the carrier's `Allow` header, and building that
Allow-sniffing plus a re-INVITE fallback is exactly the kind of speculative
machinery `specs/048`'s research.md Decision 2 already rejected for this
same feature area ("a materially larger, currently unjustified subsystem
for a header no carrier reachable here has ever sent live"). Instead: this
bridge always attempts `UPDATE`. If the carrier doesn't understand it, that
surfaces as a non-2xx/timeout response — which Decision 3 below already
treats as a fatal refresh failure, itself the correct RFC 4028 §10 outcome
for an unrefreshable session. No separate fallback path is needed; a
carrier that can't do `UPDATE` gets a clean call end instead of a silent
drop, which is the whole point of this feature either way.

## Decision 2: Refresher-role resolution — RFC 4028 §7.2, with one defensive default for a non-compliant response

**Decision**: On the outbound call's `200 OK`, if `Session-Expires` is
present:
- An explicit `refresher=uac` or `refresher=uas` parameter is used exactly
  as stated (RFC 4028 §7.2: *"The UAC MUST set the identity of the
  refresher to the value of this parameter"*).
- If `Session-Expires` is present but carries no `refresher` parameter at
  all (a non-compliant response — RFC 4028 §9 says a compliant UAS "MUST
  set the value of the refresher parameter"), this bridge defaults to
  treating **itself** as refresher (`uac`).
- If `Session-Expires` is absent entirely, there is no session-timer
  obligation (RFC 4028 §7.2: *"If the 2xx response did not contain a
  Session-Expires header field, there is no session expiration... no
  refreshes need to be sent"*) — unchanged from today.

**Rationale**: RFC 4028's own Table 2 (§9, UAS behavior) shows that when a
UAC gives no explicit preference, a compliant UAS is free to pick either
role — the RFC does not hand the UAC a rule for what to assume if the
network breaks the "MUST set refresher" requirement outright. Defaulting
to self-refresher is the defensive choice: it guarantees the call survives
regardless of what the carrier meant, at the cost of at most one harmless
extra `UPDATE` if the carrier actually intended to refresh it themselves.
The alternative (assume the carrier is refreshing) risks reproducing
exactly the silent-drop hazard this feature exists to close, on the one
input RFC 4028 leaves this bridge to decide for itself.

Note this bridge's own minimal-INVITE default (FR-009 — no
`Supported: timer` sent) means the UAC-side edge case RFC 4028 §7.2
separately describes (UAC requested a timer, UAS silently doesn't support
it, so the UAC self-imposes one) never applies here: this bridge never
requests a timer on the INVITE, so it never has that self-imposed case to
handle — every session-timer obligation this feature acts on originates
from the carrier's own `200 OK`, exactly as `spec.md`'s "purely reactive"
framing (User Story 3) describes.

## Decision 3: Refresh failure — RFC 4028 §10's per-status-code retry rule is deliberately not built; any failure is fatal

**Decision**: A sent refresh that produces a timeout, or any non-2xx final
response, ends the call. No application-level retry is attempted (matches
the `/speckit-clarify` decision already recorded in `spec.md`'s
Clarifications).

**Rationale**: RFC 4028 §10 verbatim is actually more granular than this:
timeout/`408`/`481` → send a `BYE` immediately; any *other* non-2xx →
*"SHOULD follow the rules specific to that response code and retry if
possible... SHOULD NOT continuously retry."* Building a response-code-aware
retry ladder (e.g. re-authenticate on `401`, single retry on `500`) is a
second state machine layered on top of the one this feature already needs,
for a scenario no carrier reachable here has ever been observed to trigger
at all (only Jio's `183`, which never reaches `200 OK`). The already-agreed
simplification — any failure is fatal, ending the call cleanly rather than
leaving it silently dead — delivers this feature's actual goal (no more
silent drops) without that added surface, consistent with Simplicity &
Refactorability and the same scope discipline `specs/048` applied to this
exact feature area.

## Decision 4: Timing — RFC 4028's own numbers, taken verbatim

**Decision**:
- This bridge, as refresher, sends its `UPDATE` once **half** the
  negotiated interval has elapsed (RFC 4028 §7.2/§9, both: *"It is
  RECOMMENDED that this refresh be sent once half the session interval has
  elapsed"*).
- When the carrier is refresher, this bridge gives up waiting and ends the
  call **`min(32s, interval / 3)`** before the session would expire — RFC
  4028 §10 verbatim: *"if the side not performing refreshes does not
  receive a session refresh request before the session expiration, it
  SHOULD send a BYE to terminate the session, slightly before the session
  expiration. The minimum of 32 seconds and one third of the session
  interval is RECOMMENDED."*
- Waiting for our own sent `UPDATE`'s response: a fixed, bridge-chosen
  timeout (`SESSION_REFRESH_RESPONSE_TIMEOUT`, 10s) — RFC 4028 assumes a
  full SIP transaction-timer stack (Timer F etc.) which this bridge, like
  its existing `OPTIONS` keepalive (`ping.rs`'s `PING_RESPONSE_TIMEOUT`,
  also 10s, "generous against a P-CSCF's normal response time"), does not
  implement; reusing that exact precedent's magnitude and justification
  rather than inventing a new one.
- The negotiated interval itself is treated as fixed for the life of the
  call (never renegotiated on a later refresh, even though RFC 4028
  technically permits it) — no carrier has been observed changing it
  mid-call, and building renegotiation support speculatively repeats the
  same over-building Decision 1/3 already reject.
- The carrier's stated interval is floored to RFC 4028 §9's own stated
  minimum: *"This minimum interval MUST NOT be lower than 90 seconds."*
  This bridge never gets a chance to enforce that floor through the normal
  `Min-SE`/`422` negotiation (the call is already answered by the time
  `Session-Expires` is read), so an implausibly short value is clamped up
  to 90s rather than honoured verbatim — spec.md's own Edge Cases already
  required this ("the bridge must not attempt refreshes on an interval so
  short it can't reasonably keep up"); the first implementation missed it
  (PR #74 Greptile review).

**Correction (PR #74 Greptile review)**: a refresh this bridge sent is now
resent up to `MAX_SESSION_REFRESH_ATTEMPTS` (3) times, `SESSION_REFRESH_RETRY_INTERVAL`
(3s) apart, all still within the original `SESSION_REFRESH_RESPONSE_TIMEOUT`
ceiling — the first implementation sent exactly once and let a single lost
datagram end an otherwise-healthy call. This bridge implements no SIP
transaction-layer retransmission anywhere else (`BYE`/`PRACK`/`ACK` are all
fire-and-forget too), so building full RFC 3261 timers here purely for
this one path would be disproportionate — but unlike those, losing this
one message actively tears a live call down, which earns it this bounded,
best-effort resend. Each attempt is a fresh request (new `CSeq`/branch),
not a byte-identical retransmission, which needs no new machinery: it
reuses the same `SendNow`/`on_sent` path a first attempt already takes.

The first pass at this tracked only the *latest* attempt's `CSeq`,
discarding a response to any earlier one as a "mismatch" — a second
Greptile review pass on the retry mechanism itself caught this: a late
response to the *original* attempt is a real, valid answer, not a stale
one, and dropping it could end a call the carrier had already agreed to
keep alive (e.g. original sent, retry sent, then the original's 2xx
finally arrives — discarding it and losing the retry too would wrongly
tear the call down despite the carrier having actually answered).
`AwaitingResponse` now tracks `first_cseq..=latest_cseq`, the whole range
of attempts still outstanding, and a response naming any `CSeq` in that
range settles the cycle — `PingState`'s single-pending-cseq discipline
doesn't extend cleanly to a phase with more than one attempt in flight at
once, so this feature's own state needed its own, wider rule instead of
reusing that precedent verbatim. Separately, `on_response` ignores a
provisional (`status < 200`) rather than treating it as a failure — the
first implementation would fail the whole refresh on a `100 Trying` that
a later 2xx should have resolved.

## Decision 5: Where this lives in the existing state machine

**Decision**: A new, pure, unit-testable state machine
(`agent::session_refresh::SessionRefreshState`), following the exact shape
`agent::ping::PingState`/`PingVerdict` already established (a `verdict(now)
-> Verdict` pure function, `on_sent`/`on_response`-style mutators, no I/O).
Lives as a new `Option` field on `ActiveCall` (`call.rs`), populated from
the outbound `200 OK`'s `Session-Expires` header in `origination.rs`
(threaded through `PendingOrigination` → `finish_origination`, mirroring
how every other per-call field already reaches `ActiveCall`), and consulted
once per `dispatch_loop` tick by a new `LoopState::handle_session_refresh`
method inserted alongside `handle_attachment_loss`/`handle_pbx_hangup` —
the same "one more thing to check each pass over an active call" pattern
those already establish. A response to this bridge's own sent `UPDATE`
arrives like every other carrier response, through the existing
`handle_carrier_response` dispatch (a new early-return branch, parallel to
the existing outbound-origination-response and Gm-keepalive-response
checks). A carrier-sent `UPDATE` arrives through a new, narrow
`dispatch_loop` match arm (today `UPDATE` has no arm at all and falls to
the generic `unserved_method_response` 405) that accepts it only when it
is body-less and names the active call's dialog while this bridge is
waiting on the carrier to refresh — anything else (wrong dialog, carries
SDP, arrives with no session-refresh awaited at all) falls through to the
exact same decline the generic path already sends today, unchanged.

**Rationale**: Every one of these hooks already exists for a structurally
identical purpose (attachment loss, PBX hangup, Gm keepalive) — reusing the
shape means no new control-flow primitive is introduced, matching
Simplicity & Refactorability. `end_call_attachment_lost` (`call.rs`) is
generalized to `end_call_best_effort(session, call, reason: &str)` — it
already does exactly what a session-timer-caused end needs (notify Agent B
over the control channel, best-effort `BYE` to the carrier); only its
hardcoded reason string needed to become a parameter, since FR-012 needs a
distinct reason from `ATTACHMENT_LOST`.

**Correction (PR #74 Greptile review)**: "names the active call's dialog"
above was implemented as a bare `Call-ID` comparison, which does not
actually establish dialog membership — a stale, cross-dialog, or
coincidentally-matching `Call-ID` could still reset the refresh deadline.
Fixed to reuse `names_active_dialog` (`agent/mod.rs`), the same
Call-ID-plus-both-tags check `bye_response_if_unmatched` already applies
to a `BYE`, rather than inventing a second, weaker notion of "names this
call" for `UPDATE`.

## Decision 6: Observability (FR-012) — a new `EndedBy`/`reason` pair, not a bespoke log line

**Decision**: Add `EndedBy::SessionTimerExpired` (`ims/lifecycle.rs`,
`as_str()` = `"session_timer_expired"`) and
`reason::SESSION_TIMER_EXPIRED` (`vowifi/control.rs`, value
`"session_timer_expired"`), following the exact pattern
`EndedBy::AttachmentLost`/`reason::ATTACHMENT_LOST` already establish. The
existing `report_answered_call_ended`/"call media verdict" log line and
`gsm_sip_bridge_calls_total` outcome labels already key off
`call.lifecycle.ended_by`/`call_status`, so this one new variant is
automatically picked up by every existing observability surface — no new
metric or log statement needs to be written.

**Rationale**: This bridge already has exactly the right generic mechanism
(`EndedBy` is described in `lifecycle.rs`'s own doc comment as existing so
"'the call ended' without a reason" never happens) — adding a case to it is
strictly smaller and more consistent than a bespoke log/metric addition,
and automatically shows up everywhere `ended_by`/`call_status` already do
(the "call media verdict" log, `gsm_sip_bridge_calls_total`'s outcome tag),
satisfying SC-006 for free.

## All landed code

`make format && make lint && make test` (whole workspace, clippy
`-D warnings`) — applied at implementation time, per `CLAUDE.md`.
