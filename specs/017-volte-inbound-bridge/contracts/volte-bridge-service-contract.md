# Contract: The Bridging Service

**Feature**: `017-volte-inbound-bridge` | **Satisfies**: FR-001–FR-013, FR-019–FR-022, FR-025–FR-029, FR-034–FR-037

A long-lived service holding one registration, answering calls and carrying
text messages for one card.

## Lifecycle obligations

### It holds exactly one registration

One registration serves both liveness and calls (FR-012). It MUST NOT establish
a second registration per call — that is what the one-shot diagnostic does, and
it is explicitly insufficient here.

### It never disturbs a call to maintain itself

Renewal MUST wait while a call is in progress (FR-009). **Re-attachment MUST
wait too** — the carrier tears the network attachment down roughly every two
hours and the service re-establishes it automatically, and doing that mid-call
would break the call just as surely as renewing would.

The existing Wi-Fi implementation defers renewal but knows nothing about
attachment loss, so this is the one piece of the lifecycle that is genuinely new.

### It recovers without help

When the attachment or registration is lost while idle, the service
re-establishes both and answers the next call, with no operator action
(FR-010). When they are lost *during* a call, the call ends with that named as
the cause (FR-011) — distinct from the caller hanging up — and the service
recovers afterwards.

### It says when it cannot answer

Exclusive card assignment removes the circuit-switched fallback (FR-034), so a
card here takes **no calls at all** while the service is unavailable. That makes
availability reporting load-bearing (FR-013, FR-035): an operator must be able
to see it, because nothing else will ring.

## Call obligations

| Obligation | Requirement |
|---|---|
| Answer calls arriving over the registration | FR-001 |
| Connect through to the telephone system, relaying audio both ways | FR-002 |
| Present the caller's number onward | FR-003 — the network supplies number *and* display name |
| End both legs when either ends, and report which | FR-004 |
| Give the caller a defined outcome when the telephone system cannot be reached | FR-005 |
| Reject a second concurrent call with a busy response | FR-006 — never ignore it, never disturb the call already up |
| Choose the audio format deliberately | FR-007 |

### The audio format choice is load-bearing

On the outbound path, offering narrowband first caused the carrier to select
it, and packet loss went from 0.3% to 13.6% — the network grants the
conversational-voice bearer based on what was negotiated. Answering an incoming
call carelessly reproduces that in the direction that matters more, because an
incoming call is a real conversation rather than a test.

### A silent call is a failed call

A call that connects but carries audio in only one direction, or none, MUST be
reported as a **failure** with the failing direction named (FR-017). Carried
forward from feature 016, where this rule caught a real defect. The previous
one-way-audio incident was painful precisely because a broken call looked like
a working one.

## Message obligations

### Both routes, converging

Messages arrive **over the registration** or **through the modem**, and which
one the carrier uses is its decision (research R4). Both MUST be handled,
converging on the same recording and forwarding.

Covering only one route would lose messages silently: our registration
advertises voice capability but not messaging capability, and exclusive card
assignment means the circuit-switched daemon no longer reads the modem.

### Exactly once

| Obligation | Requirement |
|---|---|
| Record and forward exactly once, whichever route delivered it | FR-037 |
| Acknowledge only *after* recording | FR-026 — a crash mid-handling must make the network retry, not lose the message |
| Clear from modem storage only after recording | FR-036 — same reasoning |
| Recognise a retransmission and not duplicate it | FR-027 |
| Record even when forwarding fails, and report the failure | FR-029 |
| Handle a message during a call without disturbing it | FR-028 |
| Recover messages already in modem storage at startup | US5 scenario 7 |

## Reuse obligations

### One implementation, not two

Registration, authentication, signalling protection, inbound handling and audio
MUST be **the same implementation** the Wi-Fi calling path uses (FR-019,
SC-008). `ims::agent` is the reuse target and must be **extracted from, not
copied**.

A copy is faster to write and satisfies neither requirement while appearing to:
two copies of registration and renewal logic would drift, and the drift would
be discovered in production on whichever path was tested less.

### The Wi-Fi path does not change

Its configuration, operational commands and observable behaviour stay as they
are (FR-020); its test suite MUST pass unmodified and a live Wi-Fi call MUST
still complete (SC-007). The modem-internal path also stays available (FR-021).

### It refuses to fight

The service MUST refuse to run while the Wi-Fi calling path holds the same
subscriber's registration (FR-022) — they present the same identity and displace
each other — and a card assigned here MUST NOT also be driven by the
circuit-switched daemon (FR-034).

## Contract tests

Pure where possible; the live column needs a carrier.

| Test | Assertion |
|---|---|
| Renewal falls due during a call | Deferred until the call ends |
| Attachment loss falls due during a call | Deferred; not torn out from under the call |
| Attachment genuinely lost mid-call | Call ends, cause is the attachment, not the caller |
| Second call while bridged | Rejected busy; the first call is undisturbed |
| Call connects, audio one way | Reported as failure, failing direction named |
| Message over registration | Recorded, forwarded, acknowledged after recording |
| Message through modem | Recorded, forwarded, cleared after recording |
| Same message on both routes | Recorded once |
| Retransmitted message | Acknowledged, not duplicated |
| Forwarding destination down | Still recorded; failure reported |
| Message during a call | Both handled |
| Wi-Fi agent already registered | Service refuses, with the reason |
