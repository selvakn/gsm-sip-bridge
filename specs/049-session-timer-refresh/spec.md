# Feature Specification: Honour RFC 4028 session-timer refresh on outbound calls

**Feature Branch**: `049-session-timer-refresh`
**Created**: 2026-09-04
**Status**: Draft
**Input**: User description: "RFC 4028 session-timer refresh (outbound/UAC leg). This bridge never advertises `Supported: timer` on outbound INVITEs, so nothing is broken today — but a carrier's `200 OK` to our own outbound call could still carry `Require: timer`/`Session-Expires` (Jio already does this, unprompted, on a `183` that happens to never reach `200 OK` — nothing guarantees the next carrier's does the same). If that ever happens on a call that actually connects, RFC 4028 §7.4 makes refreshing the session our obligation, and today `origination.rs`'s response handling never even looks at `Session-Expires`/`Require` on the final response — so a real connected call would silently drop at the session interval with zero refresh attempt. Scope: (1) full implementation — actually honour `Session-Expires`, pick a refresher, send periodic refreshes, not a defensive stub; (2) `[vowifi] originating_headers`'s `supported` token stays off by default even after this lands — this feature is purely reactive to what a carrier's response says, not a change to what we advertise."

## Why this exists

`docs/todo.md`'s longest-standing open item documents a real, unguarded gap
in this bridge's outbound (UAC) call path: the final-response handling for
a call this bridge itself placed never looks at `Session-Expires` or
`Require` on the `200 OK`. Nothing is broken today, because no carrier has
yet been caught requiring session-timer refresh on a call that actually
connects — the one carrier observed doing anything with `timer` at all
(Jio) only ever puts `Require: timer`/`Session-Expires` on a `183` that
goes on to `480` before reaching `200 OK`. But that is a fact about the one
carrier tested, not a guarantee about every carrier this bridge will ever
place a call through. If a future carrier's `200 OK` carries
`Require: timer`, RFC 4028 §7.4 makes refreshing that session this
bridge's obligation, and today it would simply `ACK` and move on — the
call would then silently drop the moment the negotiated session interval
elapsed, with no refresh ever attempted and no indication anything had
gone wrong until the audio just stopped.

This feature closes that gap with a real implementation of the session
refresh mechanics RFC 4028 defines, not a stub that merely avoids crashing
on the header. It does **not** change what this bridge advertises: the
`[vowifi] originating_headers` config's `supported` token — which already
emits `Supported: 100rel, timer` when explicitly opted in — stays off by
default before and after this feature, exactly as documented today. This
feature is purely reactive: it only changes how the bridge responds to a
carrier that volunteers session-timer terms on its own initiative,
matching the behavior already observed from Jio and the defensive posture
`docs/todo.md` calls for.

## Clarifications

### Session 2026-09-04

- Q: When a session refresh the bridge itself sends (User Story 1) gets no
  response or is rejected, should the bridge end the call on that single
  failed attempt, or get one extra try first? → A: Single attempt is
  final — no application-level retry; the underlying SIP transaction's own
  retransmission is the only retry that occurs.
- Q: Should this bridge's existing per-call metrics/logs distinguish a call
  that ended specifically because a session-timer refresh failed or never
  arrived, from other end-of-call reasons? → A: Yes — give it a distinct
  signal in the existing per-call logs/metrics.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - This bridge is the session refresher (Priority: P1)

A call this bridge placed connects, and the carrier's `200 OK` assigns the
refresh duty to this bridge (`Session-Expires` names `refresher=uac`, or
carries no `refresher` parameter and RFC 4028 §7.1's default resolution
assigns the role here). The bridge keeps the call alive by sending its own
refresh before the negotiated interval elapses, instead of letting the
call silently drop.

**Why this priority**: This is the exact scenario `docs/todo.md` flags as
the unguarded hazard — the case where staying silent has a real,
observable consequence (a connected call dropping with no warning).

**Independent Test**: Place an outbound call against a test SIP peer whose
`200 OK` carries `Session-Expires` with the bridge assigned as refresher;
hold the call open past the negotiated interval; confirm the bridge sends
a refresh and the call survives, versus dropping at the interval when this
feature is absent.

**Acceptance Scenarios**:

1. **Given** an active outbound call whose `200 OK` carried
   `Session-Expires: 300;refresher=uac`, **When** roughly half the
   interval has elapsed, **Then** the bridge sends a session refresh and
   the call continues uninterrupted past the original 300-second mark.
2. **Given** an active outbound call where this bridge is the refresher,
   **When** a sent refresh receives no response, or is answered with an
   explicit rejection, **Then** the bridge ends the call cleanly (sends
   `BYE`) rather than leaving it appearing connected with no live session.
3. **Given** an active outbound call with a pending refresh not yet due,
   **When** the call ends through any other path (far-end `BYE`, PBX
   hangup, attachment loss), **Then** no refresh is ever sent for that call
   afterward.

---

### User Story 2 - The carrier is the session refresher (Priority: P2)

A call this bridge placed connects, and the carrier's `200 OK` assigns the
refresh duty to itself (`refresher=uas`). The bridge accepts the carrier's
own in-dialog refresh request instead of rejecting it, which is what
happens today.

**Why this priority**: Without this, a carrier acting exactly as RFC 4028
prescribes — sending its own refresh — would have that refresh rejected by
this bridge (a re-INVITE on an already-connected call currently gets `488`;
`UPDATE` currently always gets `405`), killing the call the first time the
carrier tried to do the right thing.

**Independent Test**: Place an outbound call against a test SIP peer whose
`200 OK` assigns itself as refresher, have that peer send its own in-dialog
refresh before the interval elapses, and confirm the bridge accepts it
(`200 OK`) and the call continues.

**Acceptance Scenarios**:

1. **Given** an active outbound call whose `200 OK` carried
   `Session-Expires: 300;refresher=uas`, **When** the carrier sends its own
   in-dialog refresh request (carrying only a refreshed `Session-Expires`,
   no change to the negotiated media) before the interval elapses,
   **Then** the bridge accepts it and the call continues past the original
   interval.
2. **Given** the same call, **When** a request arrives on the same dialog
   that changes the negotiated media (a genuine re-negotiation, not a
   refresh), **Then** the bridge continues to decline it exactly as it
   does today — accepting refreshes must not weaken this existing
   protection.
3. **Given** the carrier assigned itself as refresher but never sends its
   own refresh, **When** the negotiated interval elapses with no valid
   refresh sent or received on either side, **Then** the bridge ends the
   call locally rather than leaving it appearing connected indefinitely.

---

### User Story 3 - The minimal-advertisement default keeps its promise (Priority: P3)

This bridge still advertises nothing extra on outbound INVITEs by default
(`originating_headers` unchanged). A carrier that volunteers
`Session-Expires`/`Require: timer` on its own initiative anyway — exactly
what Jio already does, unprompted, per `docs/todo.md` — is now handled
correctly rather than silently mishandled.

**Why this priority**: This is the framing the original request specifically
called out: this feature must not become an excuse to start advertising
`timer` support. It documents the boundary rather than adding new
behavior of its own.

**Independent Test**: Confirm an outbound call with `originating_headers`
left at its default (`[]`) still produces byte-identical INVITEs to today
(regression), while a carrier that independently sends
`Session-Expires`/`Require: timer` on the `200 OK` is now handled per User
Stories 1 and 2 rather than ignored.

**Acceptance Scenarios**:

1. **Given** `originating_headers` at its default, **When** an outbound
   call is placed, **Then** the INVITE this bridge sends is unchanged from
   today's minimal header set.
2. **Given** that same minimally-advertised call, **When** the carrier's
   `200 OK` nonetheless carries `Session-Expires`, **Then** the bridge
   still establishes and honours the refresh obligation exactly as User
   Stories 1/2 describe — the absence of an outbound `Supported: timer`
   does not suppress reacting to what the carrier actually sent back.

---

### Edge Cases

- A provisional response (`183` or other `1xx`) carrying
  `Session-Expires`/`Require: timer` that never reaches a `200 OK` (the
  only carrier behavior actually observed so far, from Jio) creates no
  refresh obligation at all — RFC 4028's session-timer mechanism applies
  to an established (`2xx`-confirmed) dialog, not a provisional one.
- A `200 OK` carries `Session-Expires` with no `refresher` parameter and no
  prior indication from either side — the refresher role must still
  resolve to a definite side per RFC 4028 §7.1's default rule, never left
  ambiguous.
- A call ends (either party's `BYE`, or any other teardown path already
  handled today) at the exact moment a refresh would otherwise be due —
  the refresh must not fire after teardown has begun.
- A `200 OK` carries an implausibly short `Session-Expires` value (below
  RFC 4028's stated interval floor) — the bridge must not attempt refreshes
  on an interval so short it can't reasonably keep up.

## Requirements *(mandatory)*

### Functional Requirements

#### Detecting and establishing the refresh obligation

- **FR-001**: The system MUST inspect the final (`200 OK`) response of
  every outbound call for a `Session-Expires` header, regardless of
  whether this bridge itself advertised any session-timer support on the
  INVITE it sent.
- **FR-002**: Whenever `Session-Expires` is present on that final response,
  the system MUST determine which side holds the refresher role, per
  RFC 4028 §7.1 (an explicit `refresher` parameter, or the applicable
  default when none is present).

#### This bridge is the refresher

- **FR-003**: The system MUST send a session refresh at or before half of
  the negotiated interval, per RFC 4028 §7.4.
- **FR-004**: If a sent refresh receives no response, or is answered with
  an explicit rejection, the system MUST end the call — a single failed
  refresh attempt is treated as final; the system does not schedule an
  additional application-level retry (the underlying SIP transaction's own
  retransmission is the only retry that occurs).
- **FR-005**: The system MUST discard any pending refresh scheduling for a
  call immediately once that call ends through any other path, so that no
  refresh is ever sent for a call that has already ended.

#### The carrier is the refresher

- **FR-006**: The system MUST accept an in-dialog session-refresh request
  from the carrier (carrying a refreshed `Session-Expires` and no change to
  negotiated media) on an already-connected outbound call, rather than
  rejecting it as it does today.
- **FR-007**: Accepting a session-refresh request MUST NOT weaken the
  system's existing rejection of requests on the same dialog that attempt
  to change the negotiated media — only a refresh-only request is
  accepted; anything else continues to be declined exactly as today.
- **FR-008**: If the negotiated interval elapses with no valid refresh
  having been sent or received on either side, the system MUST end the
  call locally, regardless of which side held the refresher role.

#### What stays unchanged

- **FR-009**: This feature MUST NOT change `[vowifi] originating_headers`'s
  `supported` token or its default value — outbound INVITEs continue to
  advertise no session-timer support unless that config is explicitly
  opted in, exactly as today.

#### Scope boundary

- **FR-010**: A provisional (`1xx`) response carrying `Session-Expires`/
  `Require: timer` MUST NOT create any refresh obligation — only a
  confirmed (`200 OK`) dialog does.
- **FR-011**: This feature applies only to calls placed over the bridge's
  SIP-signaled origination path (shared by VoWiFi and VoLTE); outbound
  calls placed over the circuit-switched path, which has no SIP dialog,
  are unaffected.

#### Observability

- **FR-012**: When a call ends specifically because a session refresh
  failed, was rejected, or never arrived in time (FR-004, FR-008), the
  system MUST record a distinct, identifiable signal in this bridge's
  existing per-call logs/metrics, so that outcome is diagnosable after the
  fact rather than indistinguishable from an ordinary hangup.

### Key Entities

- **Session Refresh State**: Per-active-call state tracking the negotiated
  refresh interval, which side holds the refresher role, and when the next
  refresh is due (or, when the carrier is the refresher, the deadline by
  which a refresh from the carrier must arrive). Exists only for the
  lifetime of the call it belongs to and is discarded the moment that call
  ends.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An outbound call where this bridge holds the refresher role
  and stays within the negotiated interval is never dropped for
  session-timer reasons, verified by holding a call open past the
  negotiated interval and confirming it survives.
- **SC-002**: A refresh that goes unanswered results in the call ending
  as soon as that single failed attempt is detected — no application-level
  retry delay — rather than the call lingering in a state that looks
  connected but no longer has a live session.
- **SC-003**: An outbound call where the carrier holds the refresher role
  and refreshes on schedule continues normally through at least one full
  refresh cycle without interruption.
- **SC-004**: A carrier that volunteers `Require: timer`/`Session-Expires`
  on a call that reaches `200 OK` no longer causes that call to silently
  drop at the session interval — the specific hazard `docs/todo.md`
  documents is closed.
- **SC-005**: With `originating_headers` left at its default, the outbound
  INVITE this bridge sends is unchanged from before this feature — no
  regression in the minimal header set every carrier in production
  receives today.
- **SC-006**: A call that ends due to session-timer refresh failure is
  identifiable after the fact from this bridge's existing logs/metrics as
  distinct from an ordinary hangup, without needing to correlate a raw SIP
  capture to explain why the call ended.

## Assumptions

- A `Session-Expires` header observed on the final response is honoured
  regardless of whether `Require: timer` accompanies it — the goal is
  preventing an unexpected call drop, matching `docs/todo.md`'s own
  defensive framing, not withholding cooperation on a technicality.
- RFC 4028 §7.1's default refresher-resolution rule applies whenever no
  explicit `refresher` parameter is present on `Session-Expires`; the exact
  mechanics of resolving it, and of the refresh transport itself (e.g.
  which SIP request carries a refresh, and any fallback between methods),
  are left to the implementation planning phase rather than fixed here.
- The minimum acceptable session interval follows RFC 4028's own stated
  floor; a `200 OK` naming an interval below that floor is treated as
  described by RFC 4028 §7.3, with exact mechanics left to the
  implementation planning phase.
- This feature covers only the bridge's SIP-signaled outbound calls
  (VoWiFi and VoLTE, which share the same origination path); the
  circuit-switched outbound path has no SIP dialog and needs no changes.
- Ending the call locally when nobody refreshes in time (User Story 2,
  Edge Cases) is a deliberate choice beyond RFC 4028's literal text,
  adopted so a call never sits indefinitely in a state that looks
  connected after a carrier silently fails its own refresher duty.
