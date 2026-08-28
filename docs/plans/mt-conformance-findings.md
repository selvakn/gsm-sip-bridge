# Terminating-side conformance findings

Tracking doc for the 2026-08-26 protocol review of the mobile-terminating
call and SMS paths (VoWiFi and VoLTE share one implementation, so every
finding applies to both) against RFC 3261/3262/3264/3428/3550/4028 and
TS 24.011/23.040/24.229/24.341/26.114/27.005. The full write-up with spec
citations, code excerpts and rationale lives in the review artifact; this
file exists to track status as fixes land, not to restate the reasoning.

Status legend: `[ ]` open, `[~]` in progress, `[x]` done, `[-]` won't fix
(with a reason).

## Batch 1 — silent losses (landed and hardware-verified 2026-08-26)

- [x] **MT-09** — reg-event NOTIFY is logged, not acted on
      (`ims/session.rs:401`). A network-side deregistration leaves the line
      reporting `Registered` and silently receiving nothing until the next
      scheduled renewal (up to an hour).
      **Landed**: `ims::session::find_own_contact` hand-parses the reginfo
      XML far enough to find our own `<contact>` (matched by IMEI substring,
      falling back to "the sole contact" when there's only one) and read its
      `state`/`event`; `contact_reports_deregistration` gates on
      `state="terminated"` with `event` in
      `deactivated`/`probation`/`rejected`/`unregistered` (`expired` is our
      own scheduled renewal, deliberately excluded). `handle_notify` now
      returns whether that happened; `agent::mod::LoopState::handle_reg_notify`
      drives `force_renewal` + `RegistrationState::Failed` off it, reusing the
      existing Gm-liveness escalation path in `on_idle_tick` rather than
      adding a second re-registration mechanism. Tests: 9 new cases in
      `ims::session::tests` (multi-contact IMEI attribution, the
      give-up-rather-than-guess case, self-closing `<contact/>`, every
      deregistration event, `expired` excluded, malformed/truncated input).
- [x] **SMS-01** — the RP message type is never checked
      (`ims/sms_pdu.rs:91`). An RP-ACK/RP-ERROR for our own outbound
      submission is walked as RP-DATA and can be forwarded as a text message
      nobody sent.
      **Landed**: `RpMessage::parse` reads the MTI first and returns a typed
      `RpMessage` (`Data`/`Ack`/`Error`); `decode_vnd_3gpp_sms` now returns
      `DecodedRp`, and only `Data` reaches the TPDU decoder. In
      `agent::mod::handle_message`, an `Ack`/`Error` is logged and answered
      `200 OK` at the SIP layer only — never decoded, never relayed, never
      forwarded to the operator. Tests: RP-ACK and RP-ERROR recognized
      without being walked as RP-DATA (`ims::sms_pdu`), every MS-to-network
      MTI rejected, plus a `decode_pdu_body`-level test (`ims::agent::mod`)
      confirming an RP-ACK body is classified `DecodedRp::Ack` rather than
      falling through to the message path.
- [x] **RTP-02** — the transcoder feeds DTMF/CN to the audio decoder
      (`ims/transcode.rs:419`). Every DTMF digit on a transcoded call is
      decoded as audio (audible artefact) and never reaches the PBX.
      **Landed**: `ChosenCodec` gained `dtmf_payload_type: Option<u8>`
      (populated from the offer's own `telephone-event` pick in
      `sdp::build_answer_for`). `relay_direction` now switches on
      `pkt.payload_type` first: the audio PT decodes as before; the DTMF PT
      is forwarded to the *far* leg's own negotiated event PT on an
      independent RTP stream (its own SSRC/seq, and a timestamp that only
      advances when the *source's* timestamp changes — i.e. a new keypress,
      not a retransmitted packet of the same one); anything else (comfort
      noise, an unrecognised PT) is dropped. Tests: event forwarded to the
      correct far-leg PT with payload preserved, repeated packets of one
      keypress keep one output timestamp, a new keypress advances it, no
      destination event PT drops cleanly, an unrecognised PT is dropped
      without reaching the decoder.
- [x] **CS-01** — modem SMS sweep uses text mode, not PDU mode
      (`volte/sms.rs:517`, `sms/reader.rs:73`). Loses UCS-2 (hex string),
      multipart (unlabelled fragments), and anything `ims::sms_pdu` already
      decodes correctly on the IMS route.
      **Landed**: the sweep now sends `AT+CMGF=0` and `AT+CMGL=4`.
      `ims::sms_pdu` gained `decode_sms_deliver_tpdu` — the same
      `SmsDeliverTpdu` parser `decode_vnd_3gpp_sms` uses, but for a bare TPDU
      with no RP-layer envelope (the shape PDU mode hands back, per
      TS 27.005 §3.1). `sms::reader::decode_pdu_line` hex-decodes the line,
      strips the length-prefixed SMSC-address field, and calls it. Tests:
      the existing `test_sms_reader.rs` fixtures rebuilt as real hex PDUs
      (8-bit/octet DCS, so the fixture text needs no GSM7 packing), plus a
      new concatenated-part case that text mode could never have represented
      (no UDH access at all).
- [x] **CS-02** — the two delivery routes decode differently, so dedupe
      doesn't match (`volte/sms.rs:91`). Falls out of CS-01 once both routes
      run the same TPDU decoder.
      **Landed**: no production change beyond CS-01 — added
      `both_delivery_routes_decode_the_same_tpdu_identically` to
      `ims::sms_pdu`'s own test module as the regression guard: the same
      TPDU decoded via `decode_vnd_3gpp_sms` (RP-DATA-wrapped, the IMS route)
      and via `decode_sms_deliver_tpdu` (bare, the modem route) must produce
      the same `sender`/`text`, which is exactly what `Dedupe`'s key needs.

All five: `make format && make lint && make test` clean (whole workspace,
including test targets). Not yet verified on real hardware — see the log
below.

## Batch 2 — say only what is true (landed 2026-08-26, pending hardware round)

- [x] **MT-03** — `Require` on an inbound request is never inspected.
      **Landed**: `ims::agent::mod::unsupported_required_extensions` reads
      every `Require` header line (not just the first) and filters against
      `SUPPORTED_EXTENSIONS` (currently empty — see MT-10). `inbound::
      handle_invite` checks it first, before even `100 Trying`, and declines
      `420 Bad Extension` with `Unsupported:` listing exactly what was
      demanded (`sip_client::build_420_bad_extension`). Tests: every tag
      listed, a request with no `Require` is untouched, multiple `Require`
      lines are all read.
- [x] **MT-10** — three different capability claims (REGISTER `Allow`,
      response `Allow`, `Supported`), none of them agreeing.
      **Landed**: one constant, `crate::ims::UAS_ALLOW`, now backs both
      `agent::mod::ALLOW` (a re-export) and `sip_client::build_register`'s
      `Allow` (`UAS_ALLOW` plus `REGISTER, SUBSCRIBE`, which this bridge
      originates itself) — REGISTER no longer claims `PUBLISH`/`UPDATE`/
      `PRACK`/`INFO`/`REFER`, none of which `dispatch_loop` has an arm for.
      The 2xx-to-INVITE's `Supported: timer, 100rel, replaces, path, gruu`
      is gone entirely — none of the five had any implementation behind
      them, and `path` doesn't even apply to a response to `INVITE`.
      `SUPPORTED_EXTENSIONS` (empty today, feeding MT-03's `Require` gate
      too) is where a future batch adds one back once it actually
      implements it. Tests: REGISTER's `Allow` no longer contains any of
      the five unimplemented methods.
- [x] **MT-07** — codec mismatch answered `486 Busy Here` instead of `488
      Not Acceptable Here`.
      **Landed**: `inbound::handle_invite`'s no-acceptable-codec decline now
      sends `sip_client::build_488_not_acceptable`, carrying `Warning: 304
      "media type not available"` (RFC 3261 §20.43). Test: the builder
      states the warning correctly.
- [x] **SMS-06** — an unsupported `MESSAGE` body is accepted rather than
      refused `415`.
      **Landed**: `agent::mod::message_content_type_supported` accepts no
      `Content-Type` at all (the long-standing plain-text default,
      unchanged) or one of `application/vnd.3gpp.sms`/`text/plain`
      (compared before any `;` parameter); anything else is refused `415`
      with `Accept:` stating what would have worked
      (`sip_client::build_415_unsupported_media`), checked at the very top
      of `handle_message` before any decode/relay/dedupe work. `message/
      cpim` unwrapping (from the original review's fix note) was left out —
      no evidence any carrier here uses it, and it's a materially bigger
      change than the refusal itself. Tests: both supported types (case-
      insensitive, with a `;charset=` parameter ignored) and no header at
      all are accepted; an unrecognised type (image/jpeg) is refused.

All four: `make format && make lint && make test` clean (whole workspace).

**Hardware-verified 2026-08-26**: rebuilt (`gsm-sip-bridge:mt-conformance-batch2`),
redeployed, real line re-registered. Two real inbound calls from the user's
phone: the first rang, was answered by the `siptest` PBX extension, ran
~20s, ended on a normal caller hangup — no `420`, no `488`, no
`Unsupported`, confirming Vi's live INVITE carries no `Require:` the new
MT-03 gate would misfire on, and that removing `Supported` didn't affect
call setup. A second attempt was abandoned by the caller before the PBX
answered (`pbx_rejected`/caller-hangup) — ordinary call abandonment,
unrelated to any batch-2 change. The `486`→`488`/`Warning` and `415`
(SMS-06) paths weren't hit by live traffic this round (no codec mismatch or
unsupported-body message arrived) — covered by unit tests only.

## Batch 3 — transactions and dialogs (landed 2026-08-26, pending hardware round)

Full spec/plan/tasks trail: `specs/042-dialog-transaction-identity/`. All
three findings share one mechanism: check an inbound request's `Call-ID`
(and, for INVITE, `CSeq`) against the single `ActiveCall` a line holds,
rather than acting on any request just because a call happens to be active.
No generic RFC 3261 §17 transaction table — the codebase's confirmed
one-call-per-line architecture makes that unjustified complexity (see
`specs/042-dialog-transaction-identity/research.md` for the full
decision/rationale/alternatives writeup per finding).

- [x] **MT-08** — in-dialog requests are not matched to a dialog (BYE tears
      down whichever call is active, regardless of `Call-ID`).
      **Landed**: `agent::mod::bye_response_if_unmatched` refuses `481 Call/
      Transaction Does Not Exist` for a `BYE` that doesn't name
      `self.active_call`'s dialog (RFC 3261 §12.2.2) — including a `BYE`
      arriving with no call active at all, which previously got an
      unconditional `200 OK` falsely implying a dialog existed.
      `handle_carrier_bye` now checks this before `self.active_call.take()`,
      instead of tearing the active call down unconditionally. Tests:
      matched/mismatched-Call-ID/no-active-call, all three in
      `agent::mod::tests`.
      **PR review fix (2026-08-26)**: the first landing matched by `Call-ID`
      alone. Greptile correctly flagged that a `BYE` reusing the active
      call's `Call-ID` with *different* dialog tags would still match and
      end the live call — a colliding or malformed Call-ID was enough,
      which is not full RFC 3261 §12.2.2 dialog identity (Call-ID plus both
      tags). Fixed with `names_active_dialog`, which additionally requires
      the request's `To` tag to equal our own (`ActiveCall::to_tag`) and its
      `From` tag to equal the caller's original one (`ActiveCall::dialog.to`,
      via a new `header_tag` parser). Test:
      `bye_response_if_unmatched_refuses_481_for_a_matching_call_id_but_different_tags`
      — the exact scenario. `CANCEL` matching (`cancel_response`) and `ACK`
      logging (`log_ack`) deliberately keep the looser Call-ID-only check:
      a `CANCEL` mirrors the original INVITE's still-untagged `To` per RFC
      3261 §9.1 and has no tag to check, and a wrongly-matched `ACK` only
      affects a diagnostic log line, not any call state.
- [x] **MT-01** — no server transaction layer (retransmission, ACK tracking).
      **Landed**, scoped to what the one-call-per-line architecture actually
      needs (not a generic transaction table):
      - `ActiveCall` gained `answered_invite: Option<CachedInviteAnswer>`
        (`agent::call`) — `Some` for a call answered as UAS, `None` for one
        this side placed itself. `classify_in_dialog_invite` compares an
        inbound INVITE's `CSeq` against the cached one: an exact match can
        only be a retransmission (RFC 3261 §12.2.2 — CSeq strictly
        increases per dialog), anything else is a genuine re-INVITE.
      - A retransmitted INVITE naming the call already answered gets the
        cached `200 OK` resent verbatim (`agent::mod::handle_inbound_invite`,
        before the busy check) instead of being reprocessed.
      - A retransmitted INVITE while still ringing gets the same `180
        Ringing` resent (`agent::inbound::await_pbx_answer`'s existing
        Call-ID-matched drain loop, extended past its CANCEL-only check)
        instead of being silently dropped.
      - A `CANCEL` naming the active (already-answered) call now gets an
        explicit `200 OK` on that call's own `To` tag
        (`agent::mod::handle_carrier_cancel`/`cancel_response`) — RFC 3261
        §9.2 requires this even though the CANCEL can no longer affect the
        call. A `CANCEL` naming anything else still falls to the existing
        `unserved_method_response` `481`, unchanged.
      - An `ACK` is now checked against the active call's `Call-ID`
        (`agent::mod::log_ack`) — a mismatch is logged (`warn!`) rather than
        silently accepted as confirming whichever call happens to be
        active. No SIP response exists for ACK either way, so this is
        diagnostics-only.
      Tests: `classify_in_dialog_invite_*` (`agent::call::tests`),
      `names_active_call_*`, `matches_caller_tag_*`,
      `names_active_dialog_*`, `cancel_response_*` (`agent::mod::tests`). The
      two retransmit-resend branches (`handle_inbound_invite`'s pre-check,
      `await_pbx_answer`'s drain loop) need a live socket/session harness
      and are hardware-verification-only, same as the existing
      CANCEL-during-ring code they extend.
      **Ruled out**: a generic RFC 3261 §17 transaction-table engine
      (transaction table, T1/T2/Timer A–K) — no evidence of concurrent
      transactions on one line; Via-branch-based transaction identity — CSeq
      equality is sufficient and RFC-grounded for the one thing that needed
      it here.
      **PR review fixes (2026-08-26)**:
      - The `await_pbx_answer` retransmit branch matched by `Call-ID` alone,
        so a *different* transaction on the same Call-ID (a distinct INVITE,
        different CSeq, arriving while the original is still ringing) was
        wrongly answered as if it were a retransmission of the original —
        resent `180 Ringing` and left with no final response of its own.
        Fixed by also requiring the CSeq to match the INVITE actually being
        rung on; anything else now falls through to the pre-existing
        log-and-drop behavior (this bridge has no better answer for that
        case — an INVITE glare scenario already ruled out of scope above —
        so it deliberately does nothing new rather than answering wrongly).
      - `handle_inbound_invite`'s pre-check had the same Call-ID-only
        exposure as MT-08's BYE bug (see there): now gated on Call-ID plus
        `matches_caller_tag` (the caller's own tag, present on every request
        in the dialog including the pre-answer retransmission, unlike our
        tag which doesn't exist yet for that case).
- [x] **MT-02** — a re-INVITE is treated as a second call and refused `486`.
      **Landed**: the same pre-check that resends a retransmitted INVITE's
      cached answer (MT-01, above) also classifies a same-Call-ID INVITE
      with a *new* CSeq as `InDialogInvite::ReInvite`, and declines it with
      `build_488_not_acceptable` — reusing the exact response shape already
      established for MT-07's codec-mismatch decline — instead of falling
      into the busy check and answering `486`. A genuinely separate,
      unrelated second call is still refused `486`, unchanged (pinned by the
      existing `a_second_call_is_rejected_busy_and_the_first_is_undisturbed`,
      which uses two different Call-IDs and continues to pass unmodified).
      **Ruled out**: actually renegotiating a call in progress (hold, codec
      change, session timers) — genuinely out of scope, this fix is about
      declining honestly, not about supporting the renegotiation; echoing
      the unchanged SDP back as a silent "accept" — rejected, misrepresents
      acceptance of a change that might be substantive; §14.2 glare — cannot
      occur, this bridge never sends its own re-INVITE.

All three: `make format && make lint && make test` clean (whole workspace,
including test targets, clippy `-D warnings`).

**Hardware-verified 2026-08-26**: rebuilt (`gsm-sip-bridge:dialog-identity`),
redeployed, real line re-registered. One real inbound call from the user's
phone: rang, answered by `siptest`'s auto-answer extension, transcoded
AMR-WB (carrier) ↔ L16 (veth) — exactly the batch-1 RTP-02 relay path,
confirming this batch's changes to `inbound.rs`'s `ActiveCall` construction
(the new `answered_invite` field) didn't disturb it — ran ~31s with
both-ways audio, and ended cleanly when the PBX side (`siptest`) hung up
first (`hangup_carrier`, not the code this batch changed, but a full
regression pass through the same `handle_inbound_invite`/`ActiveCall`
lifecycle this batch modified). Zero errors, no panics, in Agent A's own log
(`/tmp/ims-agent-0.out` inside the container — separate from `docker logs`,
which only captures Agent B).

Not exercised live, and not safely provokable without either carrier
cooperation or crafting raw SIP inside the IPsec-tunnel network namespace
against a call actually in progress (judged not worth the risk to a real
line): a BYE with a mismatched Call-ID while a call is active (MT-08's core
case — the no-active-call branch is safe to try between calls but wasn't
this round), a CANCEL after the call is already answered, and a genuine
re-INVITE (also true of MT-02, and consistent with the parent review's
finding that no carrier here has been observed sending one). All three
remain covered by the unit tests listed above; `specs/042-dialog-transaction-identity/quickstart.md`
records this constraint for the next round.

## Batch 4 — honour the negotiation (landed 2026-08-27, pending hardware round)

Full spec/plan/tasks trail: `specs/043-honour-sdp-negotiation/`. All three
SDP findings share one mechanism: `parse_offer` (`ims::sdp`) now tracks
every `m=` section an offer carries, not just a single flat audio one, so
`build_answer_for` can honestly describe what happens to each of them.
This bridge remains a single-audio-stream, plain-RTP relay by design — the
fix is answer honesty, not new relay capability (see
`specs/043-honour-sdp-negotiation/research.md` for the full
decision/rationale/alternatives writeup per finding).

- [x] **SDP-01** — media lines other than the first audio stream are
      dropped.
      **Landed**: `parse_offer` selects the **first** `m=audio` section for
      negotiation (fixing a last-wins overwrite bug: a second `m=audio`
      line previously replaced the first's port/codec list silently) and
      records every other `m=` section — another audio line, video, text,
      application, anything — as a `DeclinedMedia { kind, proto, fmts,
      before_audio }` entry, in original order. `build_answer_for` emits
      one `m=<kind> 0 <proto> <fmts>` line per entry (RFC 3264 §6: port `0`
      marks a declined stream), placed before or after the negotiated
      audio line to match the offer's own ordering. No `c=` line is added
      per declined section — the existing session-level one already covers
      it. Tests: the existing `PJSIP_REAL_VETH_OFFER` fixture (its trailing
      `m=text` section previously had zero effect on the answer) now
      asserts a declined `m=text 0 RTP/AVP 100 98` line in the right
      position; a new two-`m=audio`-section fixture proves the first wins
      and the second is declined, not silently overwritten.
      **Ruled out**: actually relaying a second audio stream, video, or
      text — this bridge is a single-audio-stream relay by design
      (`ims::sdp`'s own header comment), and no carrier here has sent more
      than one media section.
- [x] **SDP-02** — direction attributes (`sendonly`/`recvonly`/`inactive`)
      ignored.
      **Landed**: `SdpOffer` gained `direction: MediaDirection`, parsed
      from the negotiated audio section's own `a=sendonly`/`recvonly`/
      `inactive`/`sendrecv` line (default `SendRecv` if absent).
      `build_answer_for` mirrors it per RFC 3264 §6.1 instead of
      hardcoding `a=sendrecv`: `SendOnly`→`RecvOnly`, `RecvOnly`→
      `SendOnly`, `Inactive`→`Inactive`, `SendRecv`→`SendRecv`. Tests: one
      case per direction value, plus confirmation that an offer with no
      direction line (today's only real-world case) still answers
      `sendrecv` unchanged.
      **Ruled out**: gating the RTP relay's actual send/receive behavior on
      the negotiated direction — signaling correctness only; no carrier
      here has sent a non-default direction on an *initial* offer (as
      opposed to a hold re-INVITE, which batch 3 already declines
      outright), and building real per-direction suppression in
      `agent::veth`/`transcode` is a materially separate, currently
      unjustified feature.
- [x] **SDP-03** — the `m=` transport profile (`RTP/SAVP` etc.) is not
      checked.
      **Landed**: `SdpOffer` gained `proto: String`, the audio section's
      raw transport token, captured but not validated by `parse_offer`
      itself (kept permissive, same as an unrecognized codec — the caller
      decides). `agent::inbound::handle_invite` checks it immediately
      after `parse_offer` succeeds, before the existing codec precheck: a
      token other than `RTP/AVP` is declined with a new
      `sip_client::build_488_incompatible_transport`, carrying `Warning:
      305 ... "incompatible network protocol used"` (RFC 3261 §20.43) —
      visibly distinct from the existing codec-mismatch `488`/`Warning:
      304` (MT-07). Tests: the new builder states the 305 warning
      correctly; `parse_offer` captures a non-`RTP/AVP` token (e.g.
      `RTP/SAVP`) without erroring, unaffected codec parsing.
- [x] **MT-05** — session timers advertised, never honoured.
      **Confirmed resolved by prior batches — no new production code.**
      `SUPPORTED_EXTENSIONS` has been empty since MT-10 (batch 2), so the
      inbound side no longer advertises `timer` support at all — the
      finding's original premise no longer holds. RFC 4028 §9 explicitly
      permits a UAS to simply omit `Session-Expires` from its response
      when it doesn't want the extension, which is exactly today's
      behavior and is fully spec-legal. Building real session-timer
      support would mean either accepting a refresh burden this bridge
      can't fulfill (it never sends its own re-INVITE) or surviving a
      refresh re-INVITE from the far end, which collides with batch 3's
      now-unconditional `488` decline of every re-INVITE — reopening
      exactly the renegotiation scope MT-02 already ruled out, for a
      scenario no carrier here has ever sent. Test added:
      `agent::inbound::tests::session_expires_on_the_offer_is_never_echoed_back`
      pins this as intentional, not an open gap.

All three code changes plus the MT-05 test: `make format && make lint &&
make test` clean (whole workspace, including test targets, clippy
`-D warnings`).

**Hardware-verified 2026-08-27**: rebuilt (`gsm-sip-bridge:honour-sdp-negotiation`),
redeployed, real line re-registered. One real inbound call from the user's
phone: rang, answered, negotiated AMR-WB (carrier) transcoded to L16
(veth) — the same batch-1 RTP-02 relay path — with `media="both-ways"`
(586 carrier-side / 754 veth-side RX packets) and a clean `caller_hangup`
after ~15s. Zero errors, warnings, or panics in Agent A's own log
(`/tmp/ims-agent-0.out`). This exercises the ordinary path (one audio
section, no direction attribute, `RTP/AVP`) end-to-end through the
restructured `parse_offer`/`build_answer_for` — confirms no regression.

Not exercised live, and not something a real handset or this project's
carriers have been observed sending: an offer with an extra media
section, a non-default direction attribute, or an unsupported transport
profile — the three new decline paths this batch adds. All three remain
covered by unit tests only (`specs/043-honour-sdp-negotiation/quickstart.md`
records this constraint), consistent with the same posture batch 3 took
for its own low-probability live scenarios.

## Batch 5 — complete the media contract (landed 2026-08-27, pending hardware round)

Full spec/plan/tasks trail: `specs/044-complete-media-contract/`.
RTP-01 and SDP-06's `a=rtcp` half are **deferred** (see below), not
landed — reduced scope by explicit decision partway through this batch's
planning, once research showed real RTCP needs call-wide state (send-side
octet counts, an exposed/stable SSRC, a per-call timer with live socket
access, a synchronous teardown hook) that exists nowhere in this codebase
today, across all three relay call sites and both relay implementations —
a materially larger and riskier undertaking than anything else in this
review so far.

- [x] **RTP-01** — no RTCP sent or received, while its bandwidth is
      declared. **Deferred out of batch 5** — see
      `specs/044-complete-media-contract/research.md` Decision 1.
      **Landed in batch 7** (`specs/046-rtcp-reporting/`), scoped by
      explicit decision to the **carrier-facing leg of answered calls
      only** — see that batch's own entry below for what shipped and what
      remains a deliberate residue (FR-023a).
- [x] **RTP-03** — pass-through relay forwards telephone-event under the
      wrong PT.
      **Landed**: `agent::veth::forward` now reads each packet's payload
      type via the existing `rtp::parse_packet` (already used identically
      by the transcoding relay) and relabels a DTMF packet to the
      *destination* leg's own negotiated `telephone-event` payload type
      when it differs from the *source* leg's — rewriting only the
      payload-type byte (marker bit preserved), since the RFC 4733 event
      payload itself needs no re-origination the way the transcoding
      relay's own DTMF path needs one. Threaded through
      `spawn_relay`/`relay_rtp` and all four call sites (`agent/inbound.rs`,
      `agent/veth.rs`, `agent/origination.rs` ×3), reusing
      `ChosenCodec::dtmf_payload_type` from batch 1's RTP-02 fix. Tests: a
      differing-PT keypress is relabeled; a matching-PT keypress and an
      ordinary audio packet both pass through byte-for-byte unchanged.
- [x] **RTP-04** — no SSRC continuity check on receive.
      **Landed**, as observability only (see FR-005 in the spec): both
      relay implementations (`agent::veth::forward`, the pass-through
      path; `transcode::relay_direction`, the transcoding path) now log
      when a stream's SSRC changes mid-call, identifying the direction and
      old/new value — a legitimate RFC 3550 source-restart signal, never a
      reason to drop a packet or interrupt the call. Nothing in either
      relay's logic depended on SSRC continuity to function, so there was
      no existing behavior to fix, only visibility to add. Tests: a
      mid-stream SSRC change on each relay path still delivers every
      packet; a stream's first packet is never itself logged as a change.
- [x] **SDP-06** — `a=rtcp` and the offer's `ptime` are discarded.
      **Split**: the `a=rtcp` half was deferred alongside RTP-01 out of
      batch 5 (an explicit RTCP port had nothing to consume without real
      RTCP) and **landed in batch 7** alongside it — see that batch's own
      entry below. The `ptime` half **landed** here in batch 5, but not as
      originally planned: the intended fix (echo the offer's `ptime` into
      the answer, mirroring `maxptime`) turned out, on inspection, to be
      wrong — an offer's `ptime` describes what *the offer's own owner*
      intends to send, not a request for our answer to match, and this
      bridge's own packetization is a fixed, codec-level constant
      (`NegotiatedCodec::frame_samples`, unconditionally 20ms). Echoing a
      different offered value would have made the answer state a
      packetization we don't actually use — the same class of bug this
      whole review exists to eliminate, introduced anew. **Resolution**:
      confirmed via test that the answer always states its own true 20ms
      framing regardless of what any given offer's `ptime` says; no
      `SdpOffer` field added, no `build_answer_for` change.

All landed code: `make format && make lint && make test` clean (whole
workspace, including test targets, clippy `-D warnings`).

**Hardware-verified 2026-08-27**: rebuilt (`gsm-sip-bridge:complete-media-contract`),
redeployed, real line re-registered. One real inbound call from the
user's phone: rang, answered, negotiated AMR-WB (carrier) transcoded to
L16 (veth) — the transcoding relay path, which carries this batch's
RTP-04 SSRC-logging addition — with `media="both-ways"` (416 carrier-side
/ 748 veth-side RX packets) and a clean `caller_hangup` after ~15s. Zero
errors, warnings, or panics in Agent A's own log, and — correctly — no
spurious "SSRC changed" log line for this single continuous stream,
confirming the new logging stays silent on the ordinary case rather than
false-positiving.

Not exercised live: RTP-03's pass-through DTMF relabel and RTP-04's
pass-through-path SSRC logging both live in `agent::veth::forward`, which
only runs when both legs negotiate the *same* audio codec (PCMU) — this
call negotiated AMR-WB and took the transcoding path instead, same as
most real calls on this line historically. No DTMF was pressed this
round either. All three remain covered by unit tests only, consistent
with prior batches' treatment of scenarios that need a specific offer
shape no carrier here has been observed producing.

## Batch 6 — the long tail (landed 2026-08-27, pending hardware round)

Full spec/plan/tasks trail: `specs/045-long-tail-conformance/`. Four
findings — MT-06, SDP-04, SMS-05, SMS-07 — are **deferred**, not landed;
see below.

- [x] **MT-04** — `100rel` advertised but not served as a UAS.
      **Confirmed resolved by prior batches — no new code.** Same
      resolution shape as MT-05 (batch 4): `SUPPORTED_EXTENSIONS` has been
      empty since MT-10, so the inbound side no longer advertises
      `100rel` at all, and it never marks a provisional reliable (only
      plain `100`/`180`/`200`), so there is no PRACK obligation for a
      caller to serve in the first place. `Require: 100rel` is still
      declined `420` by MT-03's existing gate. Test added:
      `agent::inbound::tests::the_answering_response_never_advertises_100rel`.
- [x] **MT-11** — no `P-Access-Network-Info` on responses; SUBSCRIBE
      hardcodes Wi-Fi.
      **Landed**: `ImsRegisterConfig::access_network_info` was already
      computed correctly per line (VoWiFi: `3GPP-WLAN`; VoLTE: a real
      E-UTRAN value from the serving cell) and used for REGISTER, but
      never reached the reg-event SUBSCRIBE or the `200 OK` to an
      answered inbound INVITE. `session::SubscribeParts` gained
      `access_network_info`, echoed by `build_subscribe` in place of the
      hardcoded literal; `agent::inbound`'s per-call header list (was a
      shared `UAS_EXTRA_HEADERS` const) now carries the same real value
      into the INVITE's `200 OK`.
- [x] **MT-12** — caller identity read from `From` alone, not
      `P-Asserted-Identity`/`Privacy`.
      **Landed**: `session::extract_caller` now prefers
      `P-Asserted-Identity` (RFC 3325 — a trusted network element vouching
      for the caller) when present, falling back to `From` unchanged when
      absent. Used only for this bridge's own internal attribution (logs,
      CDRs, SMS sender fields), never re-presented to a third party, so
      `Privacy`'s onward-signaling withholding obligation doesn't apply
      here.
- [x] **MT-13** — echoed `Via` gains no `received`/`rport`.
      **Landed**: a new `sip_client::annotate_via_received_rport` adds
      `received=`/fills `rport=` per RFC 3261 §18.2.1 / RFC 3581 §4,
      applied at the two actual places a response reaches a socket
      (`SipSink::send`, `sip::server::serve`'s `send_to`) rather than
      threaded through `build_uas_response_with_headers`'s ~39 call sites
      — the real peer address is only known at the transport boundary,
      and centralizing it there touches two call sites instead of dozens.
      A no-op on anything that isn't a response, so it can't misfire on a
      request this bridge originates.
- [x] **SDP-05** — multipart bodies are parsed by accident.
      **Landed**, scoped to what the review actually found: `req.body`
      went to `sdp::parse_offer` completely unconditionally, whatever
      `Content-Type` said or didn't say. `parse_offer`'s line-scanner has
      no concept of MIME structure, so a multipart body's real SDP part
      could parse "by accident," while a malformed one could misfire in
      stranger ways (a lossy-UTF8-decoded binary sibling part producing
      spurious `m=`-prefixed "lines"). `agent::inbound::handle_invite` now
      declines anything other than `application/sdp` or no `Content-Type`
      (today's implicit assumption), same posture already established for
      `MESSAGE` bodies (SMS-06). **Ruled out**: actually parsing
      `multipart/mixed` — no evidence any carrier here sends it, and
      `ims::sdp`'s own header comment already commits it to "minimal ...
      not a general-purpose SDP library."
- [x] **SMS-02** — TP-MTI never checked; every TPDU read as SMS-DELIVER.
      **Landed**: `DecodedRp` gains `UnsupportedTpdu { rp_mr, kind }` — a
      TPDU whose own TP-MTI says SMS-SUBMIT-REPORT or SMS-STATUS-REPORT is
      now recognized before ever being walked with the SMS-DELIVER field
      layout, the same class of bug already fixed one layer up at the RP
      envelope (SMS-01). The RP-DATA itself was still received, so it's
      still owed a plain `200 OK`/RP-ACK, never relayed as a message.
- [x] **SMS-03** — no RP-ERROR path.
      **Landed**, alongside SMS-02: `DecodedRp::Undecodable { rp_mr }` (a
      TPDU that claimed SMS-DELIVER but couldn't parse) is now
      distinguished from `UnsupportedTpdu`, and gets a genuine RP-ERROR
      (new `sms_pdu::build_rp_error`, mirroring `build_rp_ack`'s shape)
      sent as a delivery report — instead of `handle_message` silently
      relaying `req.body` as if it were plain text.
- [x] **SMS-04** — message-waiting/message-class DCS groups misread.
      **Landed**, the concretely-wrong half: `Alphabet::from_dcs`'s
      `0xE0`-`0xEF` group (Message Waiting Indication, Store, UCS2 per
      TS 23.038 §4) was falling through to the GSM7 default, garbling
      real UCS2 text — added as its own branch, unconditionally UCS2 (not
      a per-bit selector, confirmed against a live decode test after an
      initial wrong assumption about the bit convention). The sibling
      Discard/Store-GSM7 groups (`0xC0`/`0xD0`) were already correct via
      the same fallback and are unaffected. Message class extraction
      itself remains out of scope — nothing downstream consumes it.
- [x] **CS-03** — no `AT+CNMI` policy asserted.
      **Landed**: `volte::sms::sweep_modem_storage` now sends
      `AT+CNMI=2,1,0,0,0` alongside its existing `AT+CMGF=0`, parity with
      the legacy multi-card pool's own init sequence
      (`modules::worker::ModuleWorker::open`), which already asserts the
      same policy. Not unit-tested — requires a real serial device, same
      constraint as the rest of this sweep's AT-command sequencing;
      verified via the hardware round below.
- [x] **CS-04** — `+CMGR` header split on commas (not quote-aware).
      **Landed**: `modules::worker::parse_sms_response`'s naive
      `line.split(',')` — correct today only by coincidence of field
      order (the `<scts>`/`<alpha>` fields with their own internal commas
      sit *after* the one field this function reads) — replaced with a
      quote-aware splitter respecting `"..."` boundaries.

**Deferred, not landed** (recorded here so the doc doesn't imply either
"done" or "won't ever do" — see `specs/045-long-tail-conformance/research.md`
Decision 1 and Decision 8 for the full reasoning):

- [ ] **MT-06** — preconditions are not implemented. Needs new SDP-level
      QoS attribute parsing and a bearer-readiness state machine; the
      header-level behavior (declining `Require: precondition`) is already
      correct via MT-03's existing gate.
- [x] **SDP-04** — an INVITE without an offer is rejected instead of
      answered with our own offer. **Landed in batch 8**
      (`specs/047-offerless-invite-sms-reassembly/`) — turned out smaller
      than this note assumed; see that batch's own entry below.
- [x] **SMS-05** — concatenated messages labelled, not reassembled. **Landed
      in batch 8** (`specs/047-offerless-invite-sms-reassembly/`), with one
      deliberate residue (the modem-storage delivery route is not yet wired
      into reassembly) — see that batch's own entry below.
- [ ] **SMS-07** — national-language shift tables unimplemented. Attempted
      and reversed mid-implementation: the mechanism (recognizing the UDH
      IEs that select a table) is small, but the part that fixes decoded
      text is TS 23.038 Annex A's character-table data, which is not
      something to ship from memory without a verifiable source — a wrong
      mapping would silently decode real text to the wrong characters,
      worse than today's honest, already-documented gap.

All landed code: `make format && make lint && make test` clean (whole
workspace, including test targets, clippy `-D warnings`).

**Hardware-verified 2026-08-27**: rebuilt (`gsm-sip-bridge:long-tail-conformance`),
redeployed, real line re-registered. One real inbound call from the
user's phone: rang, answered, negotiated AMR-WB (carrier) transcoded to
L16 (veth), `media="both-ways"` (82 carrier-side / 462 veth-side RX
packets), clean `caller_hangup`. Zero errors, warnings, or panics in
Agent A's own log around the call.

The modem-storage SMS sweep (CS-03's `AT+CNMI` addition) could not be
exercised this round — it's failing to open `/dev/ttyUSB0` at all
(`discovery error: failed to open serial /dev/ttyUSB0: No such file or
directory`), a pre-existing USB-re-enumeration quirk on this host (see
project memory: a stale `modem_port` after re-enumeration), unrelated to
this batch's code — the failure is at the serial-port-open step, before
any AT command (old or new) is ever sent. MT-11/MT-12/MT-13/SDP-05
weren't independently exercisable live either (an ordinary real call
doesn't provide a mismatched `P-Asserted-Identity`, a non-default `Via`
sent-by, or a non-SDP body to test against) — all remain covered by unit
tests only, consistent with this review's established posture for its
least-observed findings.

## Batch 7 — RTCP reporting on the carrier leg (landed 2026-08-27, pending hardware round)

Full spec/plan/tasks trail: `specs/046-rtcp-reporting/`. Closes the two
findings batch 5 deferred, scoped by explicit decision during
`/speckit-clarify` to the **carrier-facing leg of answered calls only**
(FR-023) — the internal veth leg and the originated-call path are
untouched, both deliberately, both recorded rather than silently
implied closed.

- [x] **RTP-01** — no RTCP sent or received, while its bandwidth is
      declared. **Landed**: a new `ims::rtcp` module implements RFC 3550
      SR/RR/SDES/BYE build and parse, a two-party report cadence derived
      from the declared `b=RS:800` (no member counting or timer
      reconsideration — FR-004b), and a per-call thread that sends
      periodic compound reports, reads the far end's, and sends a BYE on
      teardown. Two findings from batch 5's own deferral note turned out
      cheaper than expected on inspection (`research.md` Decisions 2 and
      3): no teardown call site needed to change at all — the RTCP thread
      owns its socket and sends its own BYE on observing the shared `stop`
      flag, after the caller has already returned — and no new timer was
      needed, since the thread's existing socket read-timeout already
      doubles as its clock. `ims::media_stats` gained `SendAccounting`
      (cumulative packets/octets/SSRC toward the carrier, mirroring
      `MediaMeter`'s existing per-counter design) and a
      `highest_extended_seq` accessor on the pre-existing `ReceiveTracker`
      — reused as-is otherwise, unifying US2's "far end's view" and US3's
      "our own receive quality" onto the codebase's one existing,
      already-tested loss/jitter implementation instead of a second one.
      Both relay implementations (`agent::veth::forward` pass-through,
      `transcode::relay_direction` transcoding) publish into the same
      per-call bundle, so a call takes either path with identical RTCP
      behaviour. Figures reach both the existing end-of-call log line and
      a new `ObservedEvent::MediaQuality`/`RtcpUnavailable` pair on the
      control protocol, feeding three new Prometheus histograms/counter
      (`gsm_sip_bridge_rtp_loss_percent`, `..._jitter_seconds`,
      `..._round_trip_seconds`, `..._rtcp_unavailable_total`).
      **One correction made during implementation**: the plan's own tasks
      described routing the far end's report *only* from a Receiver
      Report, treating a Sender Report as ignorable. That would have
      silently missed the report block on any real two-way call where the
      carrier is also transmitting audio (and therefore sending its own
      SR, not an RR) — RFC 3550 places receiver info in either packet
      type. Fixed to route a report block addressed to this bridge's own
      SSRC out of *either* wrapper; a dedicated test
      (`handle_inbound_item_routes_a_matching_block_from_either_sr_or_rr`)
      pins it.
      **Ruled out**: RFC 3550 §6.3's full multiparty scheduling (member
      counting, timer reconsideration) — every session here is two-party
      by construction; precise per-interval `fraction_lost`/LSR/DLSR in
      the report block this bridge *sends* — approximated honestly
      (cumulative loss fraction, `lsr=0` meaning "not yet correlated",
      both legal RFC 3550 values) rather than either fabricated or built
      out with full symmetric SR-tracking, which the finding's actual
      conformance obligation doesn't require.
- [x] **SDP-06** (`a=rtcp` half) — an offer's explicit RTCP port
      attribute was discarded. **Landed**: `SdpOffer` gained
      `rtcp: Option<u16>`, parsed permissively (a missing, zero, or
      unparseable value falls back to the RTP+1 convention rather than
      erroring the offer). The answer's own RTCP port follows a three-tier
      strategy — RTP+1 by convention (the answer stays byte-identical to
      before this feature — pinned by
      `a_tier_one_or_tier_three_answer_is_byte_identical_to_no_rtcp_at_all`,
      guarding against the exact class of SDP-answer regression this
      project has been burned by before), an ephemeral port declared via
      `a=rtcp:` when RTP+1 isn't available, or no RTCP at all with the
      shortfall surfaced as a warning and a metric rather than an altered
      answer (resolving a contradiction the spec's own first draft had
      between "don't claim RTCP you can't provide" and "the `b=` lines
      never change" — the clarification session caught it before planning
      started).

All landed code: `make format && make lint && make test` clean (whole
workspace, including test targets, clippy `-D warnings`). 44 new unit
tests across `ims::rtcp` (25), `ims::sdp` (6 new), `ims::media_stats` (3
new), plus the existing relay/call-site tests extended in place rather
than duplicated.

**Not yet hardware-verified** — this batch has not been rebuilt and run
against the real EC20 line. `specs/046-rtcp-reporting/quickstart.md`
records the verification plan; in particular, whether the carrier
actually sends RTCP receiver reports back is unknown until that round
runs, and is flagged there as itself a finding to record either way, not
an assumption to build on.

## Batch 8 — offerless call answering and multi-part SMS reassembly (landed 2026-08-28, pending hardware round)

Full spec/plan/tasks/research trail:
`specs/047-offerless-invite-sms-reassembly/`. Closes the two findings
batch 6 deferred as "comparable in scope to RTP-01" — both turned out
materially smaller once research checked how much of the needed state
already existed elsewhere in the codebase; see that batch's own
`research.md` for the full decision/rationale/alternatives writeup per
finding.

- [x] **SDP-04** — an INVITE without an offer is rejected instead of
      answered with our own offer. **Landed**: `agent::inbound::
      handle_invite` now recognizes an empty body (RFC 3261 §14.2/RFC 3264
      §3) before it ever reaches `sdp::parse_offer`, and routes to a new
      `handle_offerless_invite`. That function reuses `sdp::build_offer`/
      `sdp::parse_answer` **verbatim** — the exact functions
      `agent::origination` already exercises on every outbound call this
      bridge places — rather than any new SDP-building code: our own offer
      goes out in the `200 OK`, and the caller's device answers it in the
      `ACK`, awaited via a bounded drain loop over `inbound.rx` reusing the
      same shape `await_pbx_answer`'s existing CANCEL/retransmit handling
      already uses (a new `OFFERLESS_ACK_TIMEOUT`, 4s — a
      protocol-transaction-scale bound, distinct from the human-scale
      `RING_TIMEOUT`). A matching ACK with a compatible answer proceeds
      through the same RTP-connect/relay-spawn/`ActiveCall` construction
      the ordinary path already uses; an ACK that never arrives, or names
      an incompatible codec, ends the call with a `BYE` built from the
      already-available `DialogInfo::from_invite` (this codebase has no
      server-transaction layer to retract an already-sent `2xx`, so a `BYE`
      is the honest recovery, not a second final response) plus
      `ControlMessage::CallEnded` to Agent B — never a silently-connected
      or indefinitely-ringing call. `origination.rs`'s private
      `offered_chosen_codec` helper was promoted to `pub(crate)` in
      `ims::sdp` (beside `build_offer`, since the mapping is that
      function's own contract) and shared by both call sites instead of
      duplicated. Tests: the round trip (`build_offer` → a synthetic
      answer → `parse_answer` → `offered_chosen_codec`) for both offered
      codecs, an unrecognized answer rejected outright, the empty-body
      branch condition itself (including a `Content-Length: 0` and a
      whitespace-only body), and the regression guard that an ordinary
      offer-carrying INVITE is untouched.
      **Recorded scope cut — FR-002a**: the offer this path sends carries
      audio codecs only, no `telephone-event` (DTMF) and no RTCP, because
      `build_offer` has never offered either on the origination path it
      already serves, and extending it was judged real new work with no
      evidence any carrier or handset here has ever sent an offerless
      INVITE at all yet. The same class of documented residue RTP-01's
      FR-023a already established precedent for, not a silent gap.
      **Code-review fix (2026-08-28)**: the ACK-wait drain loop originally
      recognized only an `ACK`, silently ignoring anything else — including
      a `BYE`. RFC 3261 §12.1.1 confirms a UAS-side dialog the moment the
      `2xx` is sent, *before* the caller's own `ACK`, so a caller that
      answers the offer and immediately hangs up can legitimately `BYE`
      this dialog before its `ACK` ever arrives; left unanswered, that
      `BYE` would sit unacknowledged (inviting a retransmission storm)
      until the wait timed out and this bridge sent its own, redundant
      `BYE` back at a dialog the far end had already ended. Fixed: the
      drain loop now also matches a `BYE` naming this exact dialog, answers
      it `200 OK`, and skips this bridge's own teardown `BYE` in that case
      — Agent B is still told `CallEnded` and the outcome still reported,
      just without an extra, misdirected `BYE` of this bridge's own.
- [x] **SMS-05** — concatenated messages labelled, not reassembled.
      **Landed**: a prerequisite fix first — `ims::sms_pdu::
      parse_concatenation_udh` was already reading the concatenation UDH's
      reference value (TS 23.040 §9.2.3.24.1) and discarding it; it now
      returns a `ConcatPart { reference, sequence, total }`, threaded
      through `DecodedSms.part` and every call site that matched the old
      bare tuple. A new `volte::sms::Reassembly` (a near-structural twin of
      the existing `Dedupe`, sharing its `Arc<Mutex<_>>` pattern) buffers a
      multi-part message's parts, keyed on `(sender, reference)` with
      `total` checked for consistency rather than folded into the key —
      `admit_part` reports `Complete`/`Pending`/`Malformed`, and a
      `Complete` result is only cleared once its actual delivery succeeds
      (`mark_delivered`), mirroring `Dedupe::confirm`/`forget`'s own
      retry-safety shape so a failed send can recover from just the
      network's retransmission of the triggering part. `handle_message`
      (the IMS `MESSAGE` route) now runs a part through `Reassembly` between
      the existing `Dedupe` check and the `ControlMessage::SmsReceived`
      send: `Pending` acknowledges the part immediately without forwarding
      anything yet (FR-012 — the network's delivery confirmation never
      waits on reassembly); `Complete` forwards the joined text as one
      message; `Malformed` falls back to today's existing per-part labelled
      delivery. Expiry (`take_expired`, fixed at 3 minutes — the one
      `/speckit-clarify` question this batch's spec needed) piggybacks on
      `LoopState::on_idle_tick`'s existing ~1s wakeup rather than a new
      timer, flushing any held part individually, the same way a malformed
      message already falls back. 12 new unit tests for `Reassembly` alone
      (completion regardless of arrival order, two concurrent same-sender
      messages never cross-contaminating, every malformed-input case,
      idempotent retry, capacity eviction, expiry at/under/over the bound)
      plus one composing it with `Dedupe` to pin the property
      `handle_message` actually depends on.
      **Recorded scope cut**: the modem-storage (circuit-switched) delivery
      route is not yet wired into `Reassembly` — `sweep_modem_storage`
      still delivers a multi-part message's parts individually, unchanged,
      rather than calling `admit_part`. Wiring it in would mean weaving new
      state through that route's own already-intricate cross-route claim/
      relay/confirm coordination (`wait_for_resolution` and the rest of
      specs/038's reliable-delivery machinery) — judged a separate, riskier
      change from this batch's core scope rather than something to fold in
      un-reviewed. The IMS `MESSAGE` route — the one `quickstart.md` can
      actually verify live — is fully wired.
      **Code-review fix (2026-08-28)**: the first landing put the expiry
      flush in `run_modem_reader`'s own sweep loop, on the premise
      (research.md Decision 8) that "this thread's own wakeup already
      doubles as `Reassembly`'s expiry clock" — true only for a line with
      a real modem. `wants_modem_sms_reader` never spawns that thread at
      all for a `pcsc_reader` line, so a multi-part message buffered via
      `handle_message` on such a line would never expire, only ever evicted
      by `Reassembly`'s 64-entry capacity bound under an unrelated flood —
      silently violating FR-013/SC-004 for that line type. Moved to
      `LoopState::on_idle_tick`, which every line runs unconditionally
      (both call and idle cadence), fixing the gap and, incidentally,
      tightening the flush latency from ~20s to ~1s.

All landed code: `make format && make lint && make test` clean (whole
workspace, including test targets, clippy `-D warnings`): 1393 lib tests
plus every integration test suite, zero failures — re-verified after both
code-review fixes above.

**Not yet hardware-verified** — this batch has not been rebuilt and run
against the real EC20 line. `specs/047-offerless-invite-sms-reassembly/quickstart.md`
records the verification plan: SMS-05 is directly live-testable (send a
long text from the user's own phone and confirm it arrives joined, not
fragmented); SDP-04 is not — no carrier or device has been observed
sending an offerless INVITE to this line across any prior hardware round
in this whole review, so that path stays unit-test-only this round,
consistent with every prior batch's treatment of its own least-observed
scenarios.

## Hardware test log

Real-hardware verification runs on the local test rig (`test/`, see its
README) against the on-host EC20 line (Vodafone, despite any "jio" naming in
older container tags — see project memory). Each entry: what was built, what
was exercised, what was observed.

- **2026-08-26** — built `gsm-sip-bridge:mt-conformance-batch1` (all five
  batch-1 fixes) and ran it against the real EC20 line (Vi India, MCC 404
  MNC 043 — the "jio"-tagged container this line has historically run under
  is actually Vodafone/Vi, per project memory). `siptest` stood in for the
  PBX extension (registered as `1002` against `[sip_server]`, `inbound.mode
  = "answer"`).

  - **Outbound call** (`siptest call --destination <user>`): real VoWiFi
    call, answered, ~8s, both-ways audio. Confirms the redeploy is healthy
    end to end; not itself a batch-1 target.
  - **RTP-02 — confirmed live.** A real inbound call from the user's phone
    negotiated `L16` on the veth leg (carrier leg was wideband AMR-WB), so
    the transcoding relay — exactly RTP-02's code path — carried it. The
    user pressed digits during the connected call; the bridge logged
    `forwarding a DTMF keypress direction="carrier->veth" event=N` once per
    detected keypress, correctly decoding RFC 4733 event codes 8, 9, 6, 5
    (some digits reported twice — plausibly two distinct
    press-and-release reports from the handset/carrier for one keypress,
    not a bridge-side duplicate). Zero errors, no "DTMF forward failed", no
    panic, clean hangup. Before this fix these same packets were decoded as
    AMR-WB audio.
  - **CS-01/CS-02/SMS-01 — confirmed live, with one new finding.** A real
    Vi billing SMS (`VT-ViCARE-S`, alphanumeric sender — exercises the
    GSM7-packed alphanumeric TP-OA branch, not just numeric senders)
    arrived and decoded correctly via the modem-storage route. Two SMS the
    user then sent to the line arrived with plain GSM7 text intact and the
    sender decoded correctly as `+919000000000`, but **every emoji came
    through as `U+FFFD`** (confirmed via `hex(body)`: literal `EFBFBD`
    where each emoji should be). This is `ims::sms_pdu::decode_ucs2` not
    reassembling UTF-16 surrogate pairs — real phones commonly send
    astral-plane emoji this way over "UCS-2" SMS despite it predating
    surrogates, contrary to that function's docs. Pre-existing, not
    introduced by batch 1, and — notably — now broken identically on *both*
    routes (a small live confirmation of CS-02: the routes agree, including
    on this failure mode). Not one of the five requested fixes; logged
    below as a new, unscheduled finding rather than fixed inline.
  - **MT-09** not exercised — nothing in this session forced a network-side
    deregistration to test against.

### New finding from this test run

- [x] **SMS-EMOJI-01** — `decode_ucs2` (`ims/sms_pdu.rs`) didn't reassemble
      UTF-16 surrogate pairs, so any emoji outside the Basic Multilingual
      Plane was dropped as `U+FFFD` rather than decoded. Confirmed live on
      real inbound SMS from a real handset (2026-08-26).
      **Landed 2026-08-26**: `decode_ucs2` now detects a high surrogate
      (`0xD800..=0xDBFF`) followed by a low surrogate (`0xDC00..=0xDFFF`)
      and combines them into the intended code point (RFC 2781 §2.2)
      instead of decoding each unit alone; an unpaired or lone surrogate
      still falls back to `U+FFFD` rather than corrupting the rest of the
      string. Tests: direct `decode_ucs2` cases (a real surrogate pair, an
      unpaired high surrogate, a high surrogate at end of buffer, a lone low
      surrogate) plus an end-to-end TPDU-level test reproducing the exact
      shape of the message that surfaced this.
