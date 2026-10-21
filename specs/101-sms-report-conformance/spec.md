# Feature Specification: SMS Delivery Report Conformance

**Feature Branch**: `100-vowifi-underlay-guard` (spec written on the current branch by request; the spec directory number is independent of the branch name)
**Created**: 2026-10-20
**Status**: Draft
**Input**: GitHub issue #140 — on T2 Russia over VoWiFi, an incoming SMS is received and decoded, but the delivery report (RP-ACK) the bridge sends back is answered `500 Server Internal Error` ("No Response From Peer") after about 12 seconds. A MEO Portugal session accepted the same kind of report with `202 Accepted` in about 109 ms.

## Background

When an SMS arrives over IMS, 3GPP TS 24.341 §5.3.2.3 asks the receiver for two answers: a SIP response to the incoming `MESSAGE`, and a delivery report sent as a new `MESSAGE` to the IP-SM-GW (§5.3.2.4). The bridge sends both. A line-by-line comparison of the bridge's delivery-report `MESSAGE` against §5.3.2.4 and the Annex B.6 example (table B.6-7) found that everything conforms except two items:

| Element | Specification | Bridge today |
|---|---|---|
| Request-URI, From, To, Content-Type, body | §5.3.2.4 (a)–(c), (e), (f) | Conforms |
| `In-Reply-To` | §5.3.2.4 (d): "shall contain the Call-Id of the SIP MESSAGE request that was received" | **Absent** |
| First-hop `Route` | Annex B.6 table B.6-7 and TS 24.229 §5.1.2A.1.1: the P-CSCF first, then the registrar's `Service-Route` entries | **Service-Route entries only; empty when the carrier returns none** |

The `Path` header in a REGISTER `200 OK` belongs to the registrar. It is not a route the device builds for itself, so it is not what the first hop is taken from. The first hop is the P-CSCF the device already registered through.

The captured exchange cannot show *why* T2 answered `500`. This feature closes the two documented gaps; it does not claim to fix T2.

## Clarifications

### Session 2026-10-20

- Q: Should the new behaviour have a rollback setting? → A: One setting for the preloaded P-CSCF `Route` only, on by default; `In-Reply-To` is always sent.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Delivery report links back to the SMS it acknowledges (Priority: P1)

An operator with a VoWiFi or VoLTE line receives an SMS. The report the bridge sends back names the incoming message it acknowledges, as the specification requires, so a message centre that correlates reports to deliveries can do so.

**Why this priority**: It is a mandatory ("shall") requirement with no carrier risk. Where the gateway uses the header, a report without it cannot be matched to its message.

**Independent Test**: Feed the bridge a captured inbound SMS `MESSAGE` and inspect the delivery-report `MESSAGE` it produces. The report carries `In-Reply-To` equal to the inbound Call-ID, and every other header and the body are unchanged.

**Acceptance Scenarios**:

1. **Given** an inbound SMS `MESSAGE` with a known Call-ID, **When** the bridge sends its delivery report, **Then** the report contains an `In-Reply-To` header whose value is exactly that Call-ID.
2. **Given** two SMS arriving back to back with different Call-IDs, **When** both reports are sent, **Then** each report names its own message's Call-ID and not the other's.
3. **Given** a redelivery of an already-seen SMS (same Call-ID retransmitted), **When** the report is sent again, **Then** it carries the same Call-ID, and the existing de-duplication behaviour is unchanged.

---

### User Story 2 - Delivery report is routed through the P-CSCF even without a Service-Route (Priority: P1)

On a carrier whose registration returns no `Service-Route`, the delivery report is sent with the P-CSCF as its first hop, as the specification's example shows. Today the same request leaves with no `Route` at all.

**Why this priority**: This is the plausible difference between the T2 failure and the MEO success (MEO returned a Service-Route; T2 did not). It also touches every carrier, so it carries the regression risk and needs the live verification in User Story 3.

**Independent Test**: Build the report for a registration with no `Service-Route`, and for one with two `Service-Route` entries. The first has exactly one `Route` (the P-CSCF, `lr`). The second has the P-CSCF first, then both entries in their original order.

**Acceptance Scenarios**:

1. **Given** a registration with no `Service-Route`, **When** the report is sent, **Then** it carries one `Route` header naming the P-CSCF's protected server address and port with the `lr` parameter.
2. **Given** a registration with one or more `Service-Route` entries, **When** the report is sent, **Then** the P-CSCF `Route` comes first and the entries follow in the order the registrar returned them, none dropped or reordered.
3. **Given** a negotiated Gm security association, **When** the report is sent, **Then** the P-CSCF `Route` names the protected server port, and the security headers already sent on this request are unchanged.

---

### User Story 3 - No regression on carriers that already work (Priority: P1)

The two changes above alter a message that real carriers currently accept. Before merge, live traffic on Jio and Vodafone shows they still accept it.

**Why this priority**: Past incidents on these carriers (a `503` whose cause was a capability header, and a `483`/`403` whose cause was a request-line form) show that small header changes can break a working line. A unit test cannot catch that.

**Independent Test**: With the change built, receive a real SMS on a live Jio line and on the local Vodafone rig. Each delivery report gets a `2xx` and the SMS is forwarded as before.

**Acceptance Scenarios**:

1. **Given** the change deployed to a live Jio line, **When** an SMS is received, **Then** the delivery report is answered `202 Accepted` and the SMS is forwarded.
2. **Given** the change on the local Vodafone rig, **When** an SMS is received, **Then** delivery behaves exactly as before the change.
3. **Given** either carrier rejects or ignores the report after the change, **When** that is observed, **Then** the P-CSCF `Route` is restricted to registrations without a `Service-Route`, and the live check is repeated.
4. **Given** the Route setting is turned off on a live line, **When** an SMS is received, **Then** the report is sent exactly as before this feature except for `In-Reply-To`.

---

### User Story 4 - A rejected delivery report is visible (Priority: P2)

When the network answers a delivery report with a failure, the operator sees a clear warning that names the report and the status, instead of an "unmatched response" line that reads as noise.

**Why this priority**: It is how issue #140 was first noticed, and only because the reporter read raw logs. It changes no behaviour on the wire.

**Independent Test**: Simulate a `500` answer to a sent delivery report and check the log. Simulate a `202` and check there is no warning.

**Acceptance Scenarios**:

1. **Given** a delivery report has been sent, **When** the network answers with a non-2xx final response, **Then** one warning is logged that identifies the report, the status code and the reason or warning text.
2. **Given** the network answers `202 Accepted`, **When** the response arrives, **Then** no warning is logged and the existing handling is unchanged.
3. **Given** a failure answer arrives for a report, **When** logged, **Then** the bridge does not resend the report.

---

### Edge Cases

- A registration with several `Service-Route` entries: all are kept, in order, behind the P-CSCF.
- A registration where the P-CSCF address is not yet known or cannot be formed into a route: the report is still sent, with the existing route behaviour, and a warning is logged rather than the report being dropped.
- UDP versus TCP transport and IPv4 versus IPv6 P-CSCF addresses: the P-CSCF route is written correctly for each (bracketed IPv6 literals).
- A delivery report sent when the inbound message had no usable Call-ID: the report is still sent, without `In-Reply-To`, and a warning is logged.
- Late or duplicate responses to a report that has already been logged: at most one warning per report.
- SMS delivered over VoLTE (host-side LTE bridge) shares the same sending path and gets the same behaviour.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Every SMS delivery report sent in answer to an inbound IMS SMS MUST carry an `In-Reply-To` header whose value is the Call-ID of the inbound message being reported on (TS 24.341 §5.3.2.4 (d); RFC 3261 §20.21).
- **FR-002**: Every SMS delivery report MUST be sent with the P-CSCF the line is registered through as its first `Route`, with the `lr` parameter, followed by the registrar-returned `Service-Route` entries in their original order (TS 24.341 Annex B.6 table B.6-7; TS 24.229 §5.1.2A.1.1).
- **FR-003**: The first-hop `Route` MUST be taken from the P-CSCF address the line registered through, not from the registrar's `Path` header.
- **FR-004**: When a Gm security association is negotiated, the first-hop `Route` MUST name the protected server port, and the existing `Security-Verify`, `Require` and `Proxy-Require` headers MUST be unchanged.
- **FR-005**: All other parts of the delivery report MUST remain exactly as today: Request-URI from the inbound `P-Asserted-Identity`, `From`, `To`, `P-Preferred-Identity`, `Content-Type`, and the binary RP-ACK body.
- **FR-006**: When the network answers a delivery report with a non-2xx final response, the bridge MUST log one warning identifying the report, the status code, and the reason or warning text.
- **FR-007**: A rejected delivery report MUST NOT be retried (out of scope; unchanged behaviour).
- **FR-008**: The behaviour in FR-001 to FR-005 MUST apply to VoWiFi and VoLTE alike, since both use the same sending path.
- **FR-009**: The preloaded P-CSCF `Route` (FR-002 to FR-004) MUST be controlled by one configuration setting, on by default, so a deployed line can turn it off without a new build. `In-Reply-To` (FR-001) MUST always be sent and has no setting. The setting MUST be documented in the configuration reference, including that turning it off restores the previous route behaviour (Service-Route entries only).
- **FR-010**: If live verification shows the first-hop `Route` breaks a previously working carrier, the `Route` change MUST be restricted to registrations that returned no `Service-Route`, and this fallback MUST be documented in the configuration or behaviour notes.
- **FR-011**: The first-hop `Route` applies to the delivery report only. The registration-event SUBSCRIBE and every other locally originated request keep their current route behaviour.
- **FR-012**: The warning in FR-006 MUST be raised only for a final response (status 200 or above), never for a provisional (1xx) response.

### Key Entities

- **Delivery report**: the new `MESSAGE` sent to the IP-SM-GW after an inbound SMS, carrying the RP-ACK. Attributes of interest here: its own Call-ID, the `In-Reply-To` it names, and its route set.
- **Inbound SMS message**: the `MESSAGE` from the IP-SM-GW. Its Call-ID is what a report refers to.
- **Route set**: the ordered first hops a locally originated request travels through: the P-CSCF, then the registrar's `Service-Route` entries.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: In 100% of delivery reports produced by the test suite for inbound SMS, the `In-Reply-To` value equals the inbound Call-ID.
- **SC-002**: In 100% of test cases, the report's route set equals the P-CSCF followed by the registrar's `Service-Route` entries in order, including the case with none.
- **SC-003**: A byte-level comparison of a report before and after the change shows only the additions of `In-Reply-To` and the P-CSCF `Route`; no other header or body byte differs. With the Route setting off, only `In-Reply-To` is added.
- **SC-004**: On a live Jio line and on the Vodafone rig, at least 5 consecutive received SMS each get a `2xx` for their delivery report and are forwarded, matching pre-change behaviour.
- **SC-005**: Every non-2xx answer to a delivery report produces exactly one warning containing the status code; every `2xx` produces none.
- **SC-006 (reporter-validated follow-up)**: The issue #140 reporter runs a build with these changes on a T2 Russia line and posts the result. Success of this criterion is **not** required to merge, and no claim of a T2 fix is made unless it succeeds.

## Assumptions

- The bridge knows the P-CSCF address and the protected server port it registered through, because it already uses them to send every request.
- `Service-Route` entries are already stored in the order the registrar returned them.
- The MEO Portugal acceptance of a report without `In-Reply-To` means that header alone is unlikely to explain T2's `500`. Both changes are made for conformance, not as a proven T2 fix.
- No Jio or Vodafone behaviour depends on the report leaving without a P-CSCF `Route`, but this is untested on live traffic and is the main risk (see User Story 3).
- MEO Portugal and T2 Russia cannot be exercised locally; only the issue reporter can.
- Test data uses only synthetic numbers (`+919000000000`, `+919000000001`). No real identifiers appear in this spec, its tests, or its fixtures.

## Out of Scope

- Retrying or re-sending a rejected delivery report.
- Any claim that these changes fix the T2 Russia failure.
- Route changes to the registration-event SUBSCRIBE or any other request (FR-011).
- A setting for `In-Reply-To`, or separate per-carrier settings.
- Changes to the RP-ACK body, the `200 OK` answer to the inbound message, registration, or call signalling.
- Choosing a different IP-SM-GW address than the one in the inbound `P-Asserted-Identity`.
