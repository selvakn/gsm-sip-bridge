# Feature Specification: The long tail — smaller conformance gaps across SIP, SDP, and SMS

**Feature Branch**: `045-long-tail-conformance`
**Created**: 2026-08-27
**Status**: Draft
**Input**: User description: "Batch 6 conformance fixes: MT-04 (100rel advertised but not served — confirm already resolved by MT-10/MT-03, test-only), MT-11 (no P-Access-Network-Info on responses; SUBSCRIBE hardcodes Wi-Fi), MT-12 (caller identity read from From alone, never P-Asserted-Identity/Privacy), MT-13 (echoed Via gains no received/rport), SDP-05 (INVITE bodies are never checked against Content-Type before being scanned as SDP), SMS-02 (TP-MTI never checked at the TPDU layer), SMS-03 (no RP-ERROR path when a TPDU fails to decode), SMS-04 (DCS message-waiting-indication UCS2 case misread as GSM7), SMS-07 (national-language shift tables unimplemented), CS-03 (no AT+CNMI policy on the VoLTE modem-storage sweep), CS-04 (+CMGR response header split on commas without quote-awareness). MT-06 (RFC 3312 preconditions), SDP-04 (offerless INVITE), and SMS-05 (concatenated-message reassembly) are explicitly deferred to their own future features -- each needs a new subsystem (QoS/precondition state machine, pending-call state spanning the ACK, cross-message reassembly buffering) comparable in scope to RTP-01, which an earlier batch already deferred for the same reason."

## Why this exists

Batches 1-5 of this protocol-conformance review fixed the findings with the
biggest individual blast radius — silent losses, wrong response codes,
dialog/transaction identity, and the media contract. What's left
(`docs/plans/mt-conformance-findings.md`, batch 6) is smaller-impact but
still real: SIP responses that omit or misstate information a compliant
peer could reasonably rely on, SMS decoding that misreads a handful of
specific but real wire shapes, and two modem AT-command hygiene gaps.
Individually narrow; collectively worth closing in one pass since none of
them touch the same code as another.

Three findings in the original batch are **not** part of this feature,
deferred to their own future work by the same reasoning already applied to
RTP-01 (batch 5): each needs a genuinely new subsystem, not a fix to
existing logic.

- **MT-06** (RFC 3312 preconditions) needs new SDP-level QoS-negotiation
  parsing and a bearer-readiness state machine that exists nowhere in this
  codebase.
- **SDP-04** (answering an offerless INVITE with our own offer) needs new
  pending-call state that defers RTP-socket setup and codec selection from
  INVITE time to ACK time — a structural change to how a call is modeled,
  not a parsing fix.
- **SMS-05** (reassembling concatenated SMS parts into one message) needs a
  cross-message buffer keyed by sender/reference/total with its own
  eviction policy — today's decoder is stateless by design, one message in,
  one message out.
- **SMS-07** (national-language shift tables) was attempted and then
  deferred mid-implementation: the mechanism (recognizing which table an
  offer selects) is small, but the part that actually fixes what a person
  reads is the table *data* itself (TS 23.038 Annex A's character
  mappings), which is not something to ship from memory without a
  verifiable source — an incorrect mapping would silently decode real text
  to the wrong characters, worse than today's honest, already-documented
  gap.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - SIP responses state what's actually true about the call and the network (Priority: P1)

A response this bridge sends — to an inbound call or a SUBSCRIBE — should
state the caller's actual asserted identity when the network provides one,
the access network this line actually registered over (not a hardcoded
guess), and the real address a request arrived from when echoing its `Via`.

**Why this priority**: These three findings share one theme — a response
omitting or misstating something the network already told this bridge —
and are the most likely to be visible to another compliant SIP element
(the PBX, a downstream proxy) reading these responses.

**Independent Test**: Send an inbound request whose `From` differs from
its `P-Asserted-Identity`, and confirm the reported caller identity
reflects the asserted one. Confirm a VoLTE line's SUBSCRIBE states its
real access-network type, not a hardcoded Wi-Fi value. Confirm a response
whose echoed `Via` doesn't match the request's actual source address
gains a `received` parameter, and one whose top `Via` requested `rport`
gets the real port back.

**Acceptance Scenarios**:

1. **Given** an inbound request carrying both `From` and
   `P-Asserted-Identity`, **When** this bridge determines the caller's
   identity, **Then** it uses the asserted identity rather than `From`
   alone.
2. **Given** an inbound request carrying only `From` (no
   `P-Asserted-Identity`), **When** this bridge determines the caller's
   identity, **Then** behavior is unchanged from today.
3. **Given** a VoLTE line subscribing to its own registration event, **When**
   the SUBSCRIBE is built, **Then** it states that line's actual access
   network, not a hardcoded value; **When** it answers an inbound call,
   **Then** the `200 OK` states the same real access network.
4. **Given** a request whose top `Via` claims a host/port that doesn't
   match where the request actually arrived from, **When** this bridge
   responds, **Then** the echoed `Via` gains a `received` parameter naming
   the real source address.
5. **Given** a request whose top `Via` carries a bare `rport` parameter,
   **When** this bridge responds, **Then** the echoed `Via`'s `rport`
   states the real source port.

---

### User Story 2 - SMS decoding handles the specific wire shapes it currently misreads (Priority: P2)

Several TPDU/DCS shapes that can legitimately arrive are currently
misread as something else, or not distinguished from a message this
bridge already handles correctly.

**Why this priority**: Each of these produces wrong or garbled text for a
specific, narrow input shape rather than affecting every message — real
impact, but narrower than User Story 1's response-correctness issues, and
no carrier here has been observed sending most of these shapes yet.

**Independent Test**: Feed a TPDU whose type is not SMS-DELIVER (e.g. an
SMS-STATUS-REPORT) and confirm it is recognized as such rather than
misread as a delivered message. Feed a message using a message-waiting
UCS2 coding group and confirm it decodes as UCS2, not GSM7. Feed a TPDU
that fails to decode over IMS and confirm an RP-ERROR is sent back rather
than the raw undecoded bytes being relayed as if they were text.

**Acceptance Scenarios**:

1. **Given** a TPDU whose type is not SMS-DELIVER, **When** it is decoded,
   **Then** it is recognized as that type and not misread using the
   SMS-DELIVER field layout.
2. **Given** a message using a message-waiting-indication coding group
   that specifies UCS2, **When** it is decoded, **Then** its text decodes
   as UCS2.
4. **Given** an inbound IMS `MESSAGE` whose body fails to decode as a
   3GPP SMS TPDU, **When** this bridge responds at the RP layer, **Then**
   it sends an RP-ERROR rather than relaying the undecoded bytes as if
   they were the message text.

---

### User Story 3 - Modem commands and SDP bodies are validated before being trusted (Priority: P3)

The VoLTE line's modem-storage sweep should not depend on the modem's
power-on default for whether new messages are even stored; a `+CMGR`
response's fields should be parsed respecting quoting; an inbound
INVITE's body should be checked against its stated `Content-Type` before
being scanned as SDP.

**Why this priority**: All three are defensive/hygiene fixes for
conditions that haven't caused an observed failure — a modem whose
default already stores messages, a quoted field that hasn't yet collided
with a later one, a body that has, in practice, always been plain SDP.
Worth closing since they're small and self-contained, not because any has
caused an incident.

**Independent Test**: Confirm the VoLTE modem-storage sweep explicitly
sets the modem's new-message-indication policy rather than relying on its
default. Feed a `+CMGR` response whose quoted field contains a comma and
confirm the fields after it are still parsed correctly. Send an INVITE
whose body's `Content-Type` isn't SDP and confirm it's declined rather
than scanned as if it were.

**Acceptance Scenarios**:

1. **Given** the VoLTE line's modem-storage sweep starts, **When** it
   initializes the modem, **Then** it explicitly sets the new-message
   storage policy rather than relying on the modem's own default.
2. **Given** a `+CMGR` response with a quoted field containing a comma,
   **When** it is parsed, **Then** every field is attributed correctly
   regardless of that comma.
3. **Given** an inbound INVITE whose body's `Content-Type` is not SDP,
   **When** it is processed, **Then** it is declined rather than scanned
   as SDP text.
4. **Given** an inbound INVITE whose body's `Content-Type` is SDP (or
   absent, matching today's default assumption), **When** it is
   processed, **Then** behavior is unchanged from today.

---

### Edge Cases

- **MT-04 (100rel)**: confirmed already resolved by an earlier batch
  (MT-10 stopped advertising it; MT-03 already declines any `Require:
  100rel` outright) — this feature adds a confirming test only, no new
  behavior, matching how MT-05 (session timers) was resolved.
- **A request with `Privacy: id` alongside `P-Asserted-Identity`.** This
  feature reads the asserted identity for this bridge's own internal
  caller-identity attribution (logs, CDRs, SMS sender fields) regardless
  of a `Privacy` header — it does not forward or re-present identity to
  any third party, so RFC 3325's privacy-service obligations (withholding
  asserted identity from onward signaling) don't apply to this use.
- **A `+CMGR`/`+CMGL` response whose quoted `<alpha>` field itself
  contains a comma.** Covered by the same quote-aware parsing fix as
  CS-04, not a separate case.
- **An offerless INVITE, and a multipart body carrying an ISUP part.**
  Out of scope — SDP-04 (offerless) is deferred entirely; SDP-05's fix is
  a `Content-Type` check that declines what it doesn't recognize, not
  multipart parsing (no evidence this bridge's carriers send multipart
  bodies at all).

## Requirements *(mandatory)*

### Functional Requirements

#### Caller and network identity in responses

- **FR-001**: The system MUST use an inbound request's `P-Asserted-Identity`
  for this bridge's own caller-identity attribution when present, falling
  back to `From` only when it is absent.
- **FR-002**: A VoLTE line's SUBSCRIBE MUST state that line's actual
  access-network type, not a hardcoded value; a VoWiFi line's SUBSCRIBE is
  unaffected, since Wi-Fi already is its real access network.
- **FR-002b**: The `200 OK` to an answered inbound INVITE MUST state the
  line's actual access-network type, matching what its own registration
  already stated — today it states none at all.
- **FR-003**: A response's echoed `Via` MUST gain a `received` parameter
  when the request's actual source address differs from what the top
  `Via` claims.
- **FR-004**: A response's echoed `Via` MUST state the real source port
  in `rport` when the request's top `Via` carried a bare `rport`
  parameter.

#### SMS decoding correctness

- **FR-005**: A TPDU MUST be classified by its actual type before being
  decoded, and MUST NOT be walked using the SMS-DELIVER field layout when
  it is not one.
- **FR-006**: A message using a message-waiting-indication coding group
  that specifies UCS2 MUST decode as UCS2.
- **FR-008**: An inbound IMS `MESSAGE` whose body fails to decode as a
  3GPP SMS TPDU MUST receive an RP-ERROR rather than being relayed as
  plain text.

#### Modem and body validation

- **FR-009**: The VoLTE line's modem-storage sweep MUST explicitly set
  the modem's new-message storage policy rather than relying on its
  power-on default.
- **FR-010**: A `+CMGR` response MUST be parsed respecting quoted-field
  boundaries, so a comma inside a quoted field does not shift any
  subsequent field's attribution.
- **FR-011**: An inbound INVITE whose body's `Content-Type` is not SDP
  MUST be declined rather than scanned as SDP text; one whose
  `Content-Type` is SDP or absent MUST be processed exactly as today.

### Key Entities

- **Asserted identity**: The caller identity a trusted network element
  vouches for via `P-Asserted-Identity`, distinct from the caller-supplied
  `From` header.
- **TPDU type**: What TS 23.040's TP-MTI field says a given TPDU actually
  is (SMS-DELIVER, SMS-SUBMIT-REPORT, SMS-STATUS-REPORT), as opposed to
  what it's currently always assumed to be.
- **RP-ERROR**: The RP-layer failure response this bridge can now send,
  alongside the existing RP-ACK, when an inbound TPDU cannot be decoded.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: This bridge's own caller-identity attribution always
  reflects `P-Asserted-Identity` when present, across every response and
  log/CDR/SMS-sender field derived from it.
- **SC-002**: A VoLTE line's SUBSCRIBE always states its real access
  network; a VoWiFi line's is unchanged.
- **SC-003**: Every response's echoed `Via` correctly states `received`/
  `rport` whenever the request's own `Via` warrants either.
- **SC-004**: A TPDU that isn't SMS-DELIVER is never misread as one; a
  TPDU that fails to decode always produces an RP-ERROR, never a raw-bytes
  relay.
- **SC-005**: Zero regression on every existing test covering the
  ordinary (SMS-DELIVER, GSM7-default-table, From-only, no-rport) case
  across all eleven findings this feature fixes.

## Assumptions

- MT-06, SDP-04, and SMS-05 are out of scope, deferred to their own
  future features for the reasons stated above — this mirrors RTP-01's
  deferral in batch 5.
- MT-04 requires no behavior change — a confirming test only, the same
  resolution shape as MT-05.
- No carrier or device this bridge currently operates against has been
  observed sending most of the specific shapes this feature fixes
  (a message-waiting UCS2 DCS, a non-SMS-DELIVER TPDU, a non-SDP INVITE
  body) — these are correctness/interoperability fixes for gaps that
  haven't caused a live incident yet, matching the posture already taken
  for this review's other least-observed findings.
