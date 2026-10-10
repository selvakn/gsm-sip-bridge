# Feature Specification: Caller identity from `tel:` URIs

**Feature Branch**: `083-tel-uri-caller-id`
**Created**: 2026-10-10
**Status**: Draft
**Input**: User description: "Fix issue #104: caller ID extraction fails for tel: URIs. Make inbound IMS identity parsing (P-Asserted-Identity / From) conform to RFC 3261, RFC 3325 §9.1, and RFC 3966. Scope includes header_uri (SMS RP-ACK/RP-ERROR addressing): prefer the sip/sips PAI value and apply the RFC addr-spec header-param rule. A SIP URI with no userinfo yields no number."

## Why this exists

[GitHub issue #104](https://github.com/selvakn/gsm-sip-bridge/issues/104):
on T2, an inbound VoWiFi call carries the caller only as `tel:` URIs.

```
From: <tel:+919000000000;noa=international;srvattri=national>;tag=example
P-Asserted-Identity: <tel:+919000000000>
```

The bridge logs `caller=unknown`. The PBX leg, CDR and alerts all lose the
number. The bridge only recognises identities written as `sip:` URIs.

Triage found the gap is wider than the report:

- **Indian carriers already send `tel:` in `P-Asserted-Identity`.** The
  bridge silently ignores it and falls back to `From`. This skips the rule
  that the network-asserted identity wins over the caller-supplied one
  (specs/045 MT-12). That's harmless only while `From` names the same party.
  The CNAP name follows the same wrong header. An existing test for "name
  comes from PAI" passes only because `From` happens to have the same name.
- **Only the first `P-Asserted-Identity` line is read.** RFC 3325 §9.1 allows
  two values: one `sip`/`sips` and one `tel`, comma-joined or on separate
  lines. If the `tel` value is in the first line, the SIP one is never
  considered.
- `sips:` and upper-case schemes fail. So do percent-escaped (`%2B`) numbers
  and numbers written with RFC 3966 visual separators (`+91-900-…`). A
  `sip:` URI with no user part (`sip:gateway.example`) is treated as if its
  hostname were the caller's number.
- **SMS delivery reports have the same multi-value gap.** A delivery report
  (RP-ACK/RP-ERROR) goes to the URI in the message's asserted identity. With
  `<tel:…>, <sip:ipsmgw…>` the report would go to the subscriber's `tel:` URI
  instead of the SMS gateway. Separately, an unbracketed `From:
  sip:gw@host;tag=x` keeps its `;tag` as part of the address.

## Clarifications

### Session 2026-10-10

- Q: Should the SMS delivery-report addressing fix ship in this feature or
  separately? → A: In this feature, sharing one RFC-conformant parser.
- Q: What does a `sip:` URI with no user part (`sip:gateway.ims.example`)
  yield? → A: No number. It falls through to the next candidate and ends at
  "unknown". It no longer reports the hostname as the caller.
- Q: When neither `P-Asserted-Identity` nor `From` yields a number, what does
  the bridge record? → A: One warning per call/SMS, naming the raw
  `P-Asserted-Identity` and `From` values with control characters escaped.
  Nothing extra is logged on success.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Caller number shown for `tel:`-only carriers (Priority: P1)

A user on a carrier that identifies callers only by `tel:` URIs (T2) receives
a VoWiFi call. The PBX handset, the bridge's log and the call record show the
caller's number, not "unknown".

**Why this priority**: This is the reported defect. Caller ID is lost
entirely on such carriers.

**Independent Test**: Feed the bridge an inbound INVITE with the issue's
exact `From` and `P-Asserted-Identity`, and check the caller number it
reports.

**Acceptance Scenarios**:

1. **Given** an INVITE whose `P-Asserted-Identity` is
   `<tel:+919000000000>` and whose `From` is
   `<tel:+919000000000;noa=international;srvattri=national>`, **When** the
   bridge reads the caller, **Then** it reports `+919000000000`.
2. **Given** an INVITE with no `P-Asserted-Identity` and a `tel:` `From`
   carrying URI parameters, **When** the bridge reads the caller, **Then** it
   reports the number without any parameters.
3. **Given** a `tel:` URI written with visual separators
   (`tel:+91-900-000-0000`), **When** the bridge reads the caller, **Then**
   it reports `+919000000000`.

---

### User Story 2 - The network-asserted identity always wins (Priority: P1)

On any carrier, when the network vouches for the caller in
`P-Asserted-Identity`, the bridge uses that number and its display name, in
whatever valid form the carrier writes it. It uses `From` only when there is
no usable asserted identity.

**Why this priority**: This affects existing Jio/Vodafone traffic today. A
caller-supplied `From` can name a different party (an SMS gateway, a spoofed
caller), and the bridge currently trusts it whenever the asserted identity is
`tel:`.

**Independent Test**: Feed INVITEs where `P-Asserted-Identity` and `From`
name *different* numbers and names. Check that the asserted one is reported
for every valid `P-Asserted-Identity` form.

**Acceptance Scenarios**:

1. **Given** `P-Asserted-Identity: "Asserted Name" <tel:+919000000000>` and
   `From: "Other Name" <sip:+919000000001@ims.example>`, **When** the bridge
   reads the caller, **Then** the number is `+919000000000` and the name is
   "Asserted Name".
2. **Given** two `P-Asserted-Identity` lines, a `tel:` value without a name
   followed by a `sip:` value with a name, **When** the bridge reads the
   caller, **Then** it reports the asserted number and the asserted name, and
   nothing from `From`.
3. **Given** a single `P-Asserted-Identity` line holding both values
   comma-separated, **When** the bridge reads the caller, **Then** the result
   is the same as in scenario 2.
4. **Given** a `P-Asserted-Identity` with no number the bridge can read,
   **When** the bridge reads the caller, **Then** it uses `From`'s number
   *and* `From`'s name, never one from each header.
5. **Given** the caller asked for privacy (`Privacy: id`), **When** the bridge
   presents the call onward, **Then** the name is still withheld exactly as
   today.

---

### User Story 3 - SMS delivery reports reach the SMS gateway (Priority: P2)

When an SMS arrives over IMS, the bridge's delivery report goes back to the
SMS gateway named in the message, even when the carrier lists both a `tel:`
and a `sip:` identity for it.

**Why this priority**: SMS delivery currently works on Jio and Vodafone. This
protects it against a valid header form the bridge would mis-address, and
removes a stray `;tag` from a bare `From` address. The bug is latent: it's
not observed in production.

**Independent Test**: Feed SMS MESSAGE requests with various
`P-Asserted-Identity`/`From` forms and check the address chosen for the
delivery report.

**Acceptance Scenarios**:

1. **Given** `P-Asserted-Identity: <tel:+919000000000>,
   <sip:ipsmgw.example;transport=udp>`, **When** the bridge sends the
   delivery report, **Then** it addresses `sip:ipsmgw.example;transport=udp`.
2. **Given** a `P-Asserted-Identity` that is only a `tel:` URI, **When** the
   bridge sends the delivery report, **Then** it addresses that `tel:` URI,
   unchanged from today.
3. **Given** no `P-Asserted-Identity` and an unbracketed
   `From: sip:ipsmgw.example;tag=abc`, **When** the bridge sends the delivery
   report, **Then** it addresses `sip:ipsmgw.example` (the `;tag` belongs to
   the header, not the address).
4. **Given** an unbracketed `P-Asserted-Identity: sip:ipsmgw.example;lr`,
   **When** the bridge sends the delivery report, **Then** the `;lr` is kept,
   because `P-Asserted-Identity` has no header parameters.
5. **Given** the identity forms measured live on Jio and Vodafone, **When**
   the bridge sends the delivery report, **Then** the address is exactly what
   it is today.

---

### Edge Cases

- **Scheme case**: `TEL:`, `Sip:` and `SIPS:` are read the same as their
  lower-case forms.
- **`sips:` URIs** yield a number exactly like `sip:` ones.
- **`sip:` URI with no user part** (`<sip:gateway.ims.example>`) yields no
  number. With nothing else usable, the caller is "unknown".
- **Telephone-subscriber parameters in a SIP user part**
  (`sip:+919000000000;npdi;rn=+919000000099@ims.example;user=phone`) yield
  `+919000000000`.
- **Password in userinfo** (`sip:alice:secret@host`) yields `alice`. The
  password is never reported.
- **Local number with a context** (`tel:9000000000;phone-context=+91`) yields
  `9000000000`, as dialled. The context is not prepended.
- **Percent-escaped number** (`sip:%2B919000000000@…`) yields
  `+919000000000`.
- **Escapes that decode to unsafe characters** (`%3E`, `%0D%0A`, quotes,
  spaces, commas, semicolons) mean no number. The result is never embedded in
  onward signalling.
- **Display name containing URI-like text or delimiters**
  (`"sip:x <y>, z" <tel:+919000000000>`): the name is kept whole and the
  number is still `+919000000000`.
- **Non-telephony schemes** (`mailto:`, `urn:`, `data:`) yield no number.
- **Only a display name, no URI** (`"Anonymous"`) yields no number, and no
  delivery-report address.
- **Unreadable identity**: if no header yields a number, the caller is
  "unknown" and one warning records the raw header values (FR-015).
- **Compact header names** (`f:` for `From`) keep working as today.

## Requirements *(mandatory)*

### Functional Requirements

**Reading a header's values**

- **FR-001**: The bridge MUST treat every line of a repeated identity header
  as one list of values (RFC 3261 §7.3.1). It MUST split values only on
  commas outside quoted display names and outside `<…>`.
- **FR-002**: The bridge MUST parse each value as either a `name-addr`
  (optional display name, then a URI in `<…>`) or a bare `addr-spec` (RFC
  3261 §25.1). A quoted display name may contain `<`, `,` and escaped quotes
  without ending early.
- **FR-003**: For a bare `addr-spec` in a header that has header parameters
  (`From`), the bridge MUST treat everything from the first `;` on as header
  parameters, not part of the URI (RFC 3261 §20.10, §20.20). For
  `P-Asserted-Identity`, which defines no header parameters (RFC 3325 §9.1),
  it MUST keep them as part of the URI.

**Reading a number from a URI**

- **FR-004**: The bridge MUST recognise the `sip`, `sips` and `tel` schemes,
  case-insensitively (RFC 3986 §3.1). Any other scheme yields no number.
- **FR-005**: For `sip`/`sips`, the number MUST be the user part only:
  - The user part is everything before `@`. A URI without one has no user
    part and yields no number (RFC 3261 §25.1).
  - Any `:password` is dropped.
  - Telephone-subscriber parameters after the first `;` are dropped.
- **FR-006**: For `tel`, the number MUST be the telephone-subscriber before
  the first `;`. Every parameter (`ext`, `isub`, `phone-context` and
  vendor-specific ones such as `noa` and `srvattri`) is dropped (RFC 3966 §3).
- **FR-007**: The bridge MUST percent-decode the number (RFC 3261 §19.1.4,
  RFC 3966 §3). For telephone numbers (`tel`, `user=phone`, or a user part
  starting with `+`), it MUST also remove the visual separators `-`, `.`, `(`
  and `)` (RFC 3966 §5.1.1).
- **FR-008**: After decoding, the bridge MUST reject a number containing
  anything other than letters, digits and `+ * # - . _ ~`, or an empty one.
  It treats that header value as yielding no number, so caller-supplied
  escapes can never inject characters into onward signalling, records or
  logs.

**Choosing the caller identity**

- **FR-009**: The caller's number MUST come from `P-Asserted-Identity` when
  any of its values yields a number. Otherwise it comes from `From`. If
  neither yields one, it is "unknown".
- **FR-010**: Within `P-Asserted-Identity`, the number MUST come from the
  `tel` value when one yields a number, otherwise from the first `sip`/`sips`
  value that does. RFC 3325 §9.1 makes both values the same user.
- **FR-011**: The caller's display name MUST come from the same header that
  supplied the number: the first non-empty display name among that header's
  values. It MUST never combine a number from one header with a name from
  the other.
- **FR-012**: Display-name safety rules MUST stay as today:
  - names with a bare CR/LF, empty names and unterminated quotes are
    rejected;
  - `Privacy: id`/`user` still withholds the name onward.

**Addressing SMS delivery reports**

- **FR-013**: The delivery-report address MUST come from
  `P-Asserted-Identity` if it holds any URI, otherwise from `From`. Within
  the chosen header, it is the first `sip`/`sips` value if any, otherwise the
  first value with a URI. The URI is complete (with its URI parameters), and
  a bare `From` follows FR-003.
- **FR-014**: For every identity form measured live on Jio and Vodafone
  (bracketed `sip:` with URI parameters, bracketed host-only `From` with a
  `;tag`), the delivery-report address MUST be identical to today's.

**Diagnosing failures**

- **FR-015**: When neither `P-Asserted-Identity` nor `From` yields a number
  for an inbound call or SMS, the bridge MUST log exactly one warning for
  that request (a retransmission of it is the same request and MUST NOT
  log again), naming:
  - the raw `P-Asserted-Identity` values (or that the header is absent);
  - the raw `From` values (or that the header is absent).

  Control characters in those values MUST be escaped so they cannot forge or
  split log lines. A request whose number resolves MUST NOT produce this
  warning. Neither must an SMS whose sender is already known from the
  message body itself.

### Key Entities

- **Identity header value**: one entry of a `P-Asserted-Identity` or `From`
  header. It has an optional display name and a URI.
- **Caller identity**: the number and optional display name the bridge
  attributes to an inbound call or SMS, taken from a single header.
- **Delivery-report address**: the full URI an SMS delivery report is sent
  to.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: An inbound call with the issue's exact headers is attributed to
  `+919000000000`, not "unknown". This is checked against a unit fixture and,
  after release, confirmed by the reporter on T2.
- **SC-002**: Every valid `P-Asserted-Identity` form in RFC 3325 §9.1
  produces the asserted number and name, even when `From` names a different
  party:
  - one value (`sip`/`sips`/`tel`)
  - two values on one line
  - two values on two lines, in either order
- **SC-003**: Every edge case listed above has a test with the stated
  outcome, and no case reports a hostname, password or URI parameter as part
  of a number.
- **SC-004**: No caller-supplied value can make the reported number contain
  a quote, angle bracket, comma, semicolon, whitespace or line break.
- **SC-005**: A live inbound call on the Vodafone rig shows the same caller
  number and CNAP name on the PBX leg as before the change.
- **SC-006**: A live SMS on the Jio line produces a delivery report to the
  same gateway address as before the change.
- **SC-007**: An inbound call whose identity headers are unreadable leaves
  exactly one warning that contains both raw header values, with any
  embedded line breaks escaped. A call with a readable identity leaves none.
- **SC-008**: All existing caller-ID, CNAP, privacy and delivery-report tests
  still pass. The one test that passed by accident is corrected so it would
  fail if PAI were ignored.

## Assumptions

- Calls and SMS over VoWiFi/IMS are in scope. Caller ID on the GSM
  (circuit-switched) path comes from the modem, not SIP headers, and is
  unaffected.
- The PBX-facing side's own URI parsing (which deliberately ignores `tel:`)
  is out of scope.
- A local `tel:` number is reported as dialled. No carrier we know of sends
  local numbers as an inbound caller identity, and RFC 3966 does not define
  combining the number with its context as equivalent.
- In a two-value `P-Asserted-Identity`, both values name the same subscriber
  (RFC 3325 §9.1). Preferring `tel` for the number changes nothing visible
  when both are present, and keeps the canonical E.164 form.
- T2 cannot be reached from the test rigs. SC-001 relies on the issue's
  captured headers plus the reporter's confirmation after release.
- No configuration options are added. This is a conformance fix.
