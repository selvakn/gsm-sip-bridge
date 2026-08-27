# Phase 0 Research: The long tail — SIP, SDP, and SMS conformance gaps

No `NEEDS CLARIFICATION` markers were left in the spec. Three Explore
passes (SIP headers, SDP, SMS/CS) plus targeted direct verification
resolved every finding's exact current state and mechanism.

## Decision 1: MT-06, SDP-04, SMS-05 are deferred — each needs a new subsystem

**Decision**: Not attempted in this feature. Recorded in
`docs/plans/mt-conformance-findings.md` as deferred, matching RTP-01's
treatment in batch 5.

**Rationale**:
- **MT-06** (RFC 3312 preconditions) needs new SDP-level QoS attribute
  parsing (`a=curr`/`a=des`/`a=conf`) and a bearer-readiness state machine
  — nothing like it exists. The header-level behavior (declining
  `Require: precondition` outright) is already correct via MT-03's
  existing blanket gate; there is no narrow gap left to close short of
  the whole subsystem.
- **SDP-04** (answering an offerless INVITE with our own offer) needs a
  second, materially different path through `handle_invite`: defer
  RTP-socket connect, codec selection, and relay-spawning from INVITE
  time to ACK time, which requires new pending-call state in
  `agent::mod`'s dispatch loop and `agent::call::ActiveCall` (today,
  media setup happens synchronously inside `handle_invite`, before any
  ACK exists) plus a real ACK-body-parsing path (`log_ack` today does
  nothing with `req.body`).
- **SMS-05** (concatenated-message reassembly) needs a cross-message
  buffer keyed by (sender, reference number, total) with its own
  eviction/timeout policy — `sms_pdu.rs` is a pure, stateless decoder by
  design (one call in, one message out); reassembly is the one finding
  here that cannot be done inside that shape.

All three are comparable in structural weight to RTP-01, which batch 5
already established the precedent for deferring rather than force-fitting
into a batch alongside genuinely small fixes.

## Decision 2: MT-04 is a confirming test, not new behavior — same as MT-05

**Decision**: No code change. A test pins that `100rel` is neither
advertised on any UAS response nor obligated by anything this bridge
sends (no `183`, no `RSeq`), and that `Require: 100rel` on an inbound
INVITE is still declined by MT-03's existing gate.

**Rationale**: `SUPPORTED_EXTENSIONS` has been empty since MT-10; the
inbound/UAS side sends only `100`/`180`/`200` (never a reliable `183`), so
it never creates a PRACK obligation for a caller to serve. The only place
`100rel` is still advertised is the outbound/UAC side (`call.rs:740`),
which already serves it correctly (`origination::prack_if_required`) —
that side isn't part of this finding. Same premise-no-longer-holds
resolution as MT-05 in batch 4.

## Decision 3: MT-11 — thread the real access-network value through both places it's missing

**Decision**: `SubscribeParts` (`session.rs`) gains
`access_network_info: &'a str`; `build_subscribe` echoes it instead of
the hardcoded `P-Access-Network-Info: 3GPP-WLAN` literal. `subscribe_reg_event`
gains a parameter threading it from `reg_cfg.access_network_info`, already
in scope at both call sites (`agent/mod.rs:731,2172`, both inside
functions holding `p.reg_cfg`/an `ImsRegisterConfig`). Separately,
`UAS_EXTRA_HEADERS` (`agent/inbound.rs`) becomes a small per-call `Vec`
(built once in `handle_invite`, not a `&'static` const) so the `200 OK` to
an answered inbound INVITE can carry the same value.

**Rationale**: `ImsRegisterConfig::access_network_info` already exists and
is already correctly computed per access type (VoWiFi:
`ACCESS_NETWORK_WLAN`; VoLTE: a real value from the serving cell,
`volte::pani`) — it's used correctly for the REGISTER itself
(`ims/mod.rs:829-832`) and simply never reaches the SUBSCRIBE or the UAS
response. No new computation, only wiring what's already computed to two
more places.

**Alternatives considered**:
- Store `access_network_info` on `RegisteredSession` — rejected: every
  call site that needs it already has `reg_cfg`/`ImsRegisterConfig`
  directly in scope, so adding a session field would just be a second copy
  of the same value with no new capability.

## Decision 4: MT-12 — a P-Asserted-Identity-preferring sibling to `extract_caller`

**Decision**: `extract_caller` (`session.rs`) tries
`P-Asserted-Identity` first (same user-part extraction `From` already
uses), falling back to `From` when absent. `Privacy` is not consulted —
this bridge only uses the result for its own internal attribution (logs,
CDRs, SMS sender fields), never re-presenting it to a third party, so
RFC 3325's privacy-service obligations (withholding asserted identity
from onward signaling) don't apply to this use.

**Rationale**: The exact URI-user-part parsing `extract_caller` already
does for `From` applies unchanged to `P-Asserted-Identity` — same header
shape, same extraction. `header_uri` (the sibling helper used elsewhere
for addressing SMS delivery reports) already demonstrates this codebase's
established pattern of a generic-header-then-specific-fallback helper.

**Alternatives considered**:
- Honor `Privacy: id` by suppressing the asserted identity even
  internally — rejected: nothing in this codebase forwards caller
  identity onward to any third party (it's an inbound relay, not a proxy
  re-presenting identity), so there is no privacy-service context in
  which RFC 3325 §9.1's withholding obligation would apply.

## Decision 5: MT-13 — annotate at the transport boundary, not at every response builder

**Decision**: A new pure function,
`sip_client::annotate_via_received_rport(message: &str, peer: SocketAddr) -> String`,
inspects only the top `Via` of a response (`message.starts_with("SIP/2.0 ")`
guards against ever touching a request we originate) and adds
`received=<peer.ip()>` when the stated sent-by host differs from `peer`,
and fills a bare `;rport` with `;rport=<peer.port()>`. Applied at exactly
two places — the only two places a response is actually written to a
socket: `SipSink::send` (a new `SipSink::peer_addr()` accessor supplies
`peer`, using `TcpStream::peer_addr()` for TCP and the already-stored
peer for UDP) and `sip::server::serve`'s one `socket.send_to(...)` call
site.

**Rationale**: `build_uas_response_with_headers` has ~39 call sites across
`agent/inbound.rs`, `agent/mod.rs`, and `sip/server/mod.rs`, and has no
access to the real peer address today — but RFC 3261 §18.2.1 assigns this
job to whatever received the request and knows its real source, which in
this codebase is exactly the two socket-facing send points, not the
many places that build response text. Doing it there means two call
sites, not thirty-nine, and no signature change to a function this many
tests already call directly with fixture requests (those tests are
unaffected — they never touch `SipSink`/`serve`, so nothing about them
needs to change).

**Alternatives considered**:
- Add a `peer: Option<SocketAddr>` parameter to
  `build_uas_response`/`build_uas_response_with_headers` and thread it
  through every call site — rejected: the same information is available
  at exactly two natural choke points already; doing it there is strictly
  less code and touches zero existing tests.

## Decision 6: SMS-02/SMS-03 — extend `DecodedRp`, don't change `decode_sms_deliver_tpdu`'s contract

**Decision**: `DecodedRp` gains two variants, parallel to the existing
`Ack`/`Error`:
- `UnsupportedTpdu { rp_mr: u8, kind: TpduMessageType }` — the RP-DATA
  envelope was fine, but its own TP-MTI says the TPDU inside isn't
  SMS-DELIVER (an SMS-SUBMIT-REPORT or SMS-STATUS-REPORT, TS 23.040
  §9.2.3.1). Recognized, not garbled — nothing to relay, same "nothing to
  forward" treatment as `Ack`/`Error`. The caller sends a plain `200 OK`
  (an RP-ACK would be a lie about what was "delivered"), never relays it
  as text.
- `Undecodable { rp_mr: u8 }` — the TPDU claimed to be SMS-DELIVER but its
  bytes don't parse as one (truncated, malformed). This is a genuine
  failure, and (SMS-03) now gets an **RP-ERROR** sent back
  (`sms_pdu::build_rp_error`, mirroring `build_rp_ack`'s existing shape)
  instead of the request being silently relayed as if `req.body` were
  plain text.

`decode_vnd_3gpp_sms` classifies TP-MTI (a new `TpduMessageType` — new
small enum, `first_octet & 0x03`) before ever walking TP-OA/TP-PID/etc.,
and only falls through to the existing `SmsDeliverTpdu::parse` for a
genuine SMS-DELIVER. `handle_message` (`agent/mod.rs`) gets two new match
arms, both returning early exactly like the existing `Ack`/`Error` arms
do — neither reaches the `body = req.body.clone()` fallback that today
conflates "not a 3GPP body at all" (legitimately plain text) with "was a
3GPP body that failed to decode" (should never be relayed as text).

**Rationale**: TS 23.040's SC→MS TP-MTI values (`00` DELIVER, `01`
SUBMIT-REPORT, `10` STATUS-REPORT, `11` reserved) have completely
different field layouts after the first octet — walking a
SUBMIT-REPORT/STATUS-REPORT with `SmsDeliverTpdu::parse`'s TP-OA/TP-PID/
TP-DCS/TP-SCTS/TP-UDL layout is exactly the RP-layer bug (SMS-01,
already fixed) recurring one layer down: a garbled slice that
occasionally still parses, producing a plausible but wrong sender/text
for something that was never a deliverable message. The existing
`Ack`/`Error` variants are the established pattern for "recognized,
nothing to forward" at the RP layer; this extends the identical pattern
one layer down, rather than inventing a second mechanism.

**Alternatives considered**:
- Return a bare `Err(String)` from `decode_sms_deliver_tpdu` for a
  non-DELIVER TPDU, same as a malformed one — rejected: loses `rp.mr`
  (needed for the RP-ERROR SMS-03 sends on a genuine failure) and
  conflates "recognized, not actionable" with "actually broken," which
  need different responses (`200 OK` vs. RP-ERROR).

## Decision 7: SMS-04 — fix the message-waiting-indication group's alphabet bit, not just document the gap

**Decision**: `Alphabet::from_dcs` (`sms_pdu.rs`) adds the missing `else
if dcs & 0xF0 == 0xE0` branch (Message Waiting Indication, Store, TS
23.038 §4): bit 2 selects UCS2, matching the existing `0xF0` (Data
coding/message class) branch's own bit-2 convention. The `0xC0`/`0xD0`
groups (Discard / Store-GSM7) are unaffected — they already fall through
to the correct `Gsm7` default.

**Rationale**: `0xE0`-`0xEF` (Store Message, UCS2) is the one DCS group in
this table that is *concretely* wrong today (decoded as GSM7, garbling
the text) rather than merely unhandled-and-falling-back-correctly
(`0xC0`/`0xD0` are also unhandled but their fallback happens to be
right). This is the same class of bug the UCS2-emoji fix
(`SMS-EMOJI-01`, discovered live 2026-08-26) already fixed one layer
down — decoding UCS2 bytes as if they were GSM7 septets.

**Alternatives considered**:
- Also extract `message_class`/MWI-active/type into new `DecodedSms`
  fields — deferred: nothing downstream consumes a message class or MWI
  state today (this bridge doesn't drive a phone's MWI lamp or per-class
  storage behavior), so adding unused fields would be exactly the
  half-finished-addition pattern this project's conventions warn against.
  The alphabet fix is the part that actually changes what text a person
  reads; the rest stays documented as out of scope, same as before.

## Decision 8: SMS-07 — implement the national-language shift tables, not just document the gap

**Decision**: `sms_pdu.rs` gains the national-language single-shift and
locking-shift tables (TS 23.038 §6.2.1.2/6.2.1.3, Annex A) actually used
in practice — recognizing the UDH IEs that select them (`0x24`
single-shift, `0x25` locking-shift, TS 23.040 §9.2.3.24.10/.24.11,
currently silently skipped by the UDH walker's catch-all) and decoding
`0x1B`-escaped septets through the selected table instead of always the
default extension table.

**Rationale**: The module's own doc comment already flags this as a known
gap ("vanishingly rare in practice"), and the escape-handling code has a
concrete, if narrow, failure mode today: an escape sequence a national
table defines but the default table doesn't hits the `_ => { bad_escape
= true; ' ' }` arm, decoded as a literal space — and enough of those can
even mislead the "is this actually unpacked ASCII" recovery heuristic
elsewhere in the same file. Each TPDU carries its own table-selection IE,
so this is resolvable within one decode call — no cross-message state
needed, unlike SMS-05.

**Scope boundary**: only the language tables with real-world traffic
justifying them (per TS 23.038 Annex A — Turkish, Spanish/Portuguese) are
added, not the full generic table-selection mechanism for every locale
TS 23.038 defines. Extending to more tables later is adding table data,
not new mechanism.

## Decision 9: CS-03/CS-04 — small, independent fixes in the two circuit-switched code paths

**Decision**:
- **CS-03**: `volte::sms::sweep_modem_storage` adds `AT+CNMI=2,1,0,0,0`
  to its existing brief PDU-mode session, alongside the `AT+CMGF=0` it
  already sends — the same policy the separate legacy multi-card pool
  (`modules::worker::ModuleWorker::open`) already asserts, so this is
  parity with an existing, working convention, not a new policy choice.
- **CS-04**: `modules::worker::parse_sms_response`'s naive `line.split(',')`
  is replaced with a quote-aware splitter (respects `"..."` boundaries)
  before extracting the sender field, so a quoted field containing a
  comma (`<scts>`'s own internal comma, or a SIM-phonebook `<alpha>` name
  like `"Doe, John"`) can never shift a later field's attribution.

**Rationale**: Both are narrow, single-function fixes in two already-
identified locations, with no design ambiguity — CS-03 mirrors an
existing convention verbatim, and CS-04 is a well-understood
quote-awareness fix (TS 27.005 §3.1's `+CMGR` line already has quoted
fields; the fix makes the parser actually respect them instead of
assuming none exist, which happens to be harmless today only because of
which field this parser currently reads).

## Decision 10: SDP-05 — a `Content-Type` gate on the INVITE body, not multipart parsing

**Decision**: `agent::inbound::handle_invite` checks the INVITE's
`Content-Type` before calling `sdp::parse_offer` — accepting `application/sdp`
or no `Content-Type` at all (today's implicit assumption), declining
anything else (including `multipart/mixed`) with the same shape of
response `message_content_type_supported`/`415` already established for
`MESSAGE` bodies (SMS-06, batch 2).

**Rationale**: `req.body` is handed to `parse_offer` completely
unconditionally today — no `Content-Type` check exists anywhere in
`handle_invite`. `parse_offer`'s line-scanner has no concept of MIME
structure, so a multipart body's boundary/part-header lines are silently
skipped as unrecognized, and its SDP part's real `m=`/`c=`/`a=` lines are
indistinguishable, to that scanner, from a bare SDP body's own lines —
which is why a well-formed multipart body might parse "by accident" today
(the finding's own wording), and why a non-SDP or malformed-multipart
body can misfire in the more specific way the research identified (a
lossy-UTF8-decoded binary sibling part producing spurious `m=`-prefixed
"lines"). Declining what isn't recognized is the same posture already
established for `MESSAGE` bodies, applied here for the first time to
`INVITE`.

**Alternatives considered**:
- Actually parse `multipart/mixed` (boundary-splitting, per-part
  `Content-Type` selection of the SDP part) — rejected: no evidence any
  carrier this bridge talks to sends multipart bodies; the module's own
  header comment already commits `ims::sdp` to "minimal ... not a
  general-purpose SDP library," and boundary-parsing is a materially
  bigger addition than a `Content-Type` check for a scenario with zero
  observed occurrence.
