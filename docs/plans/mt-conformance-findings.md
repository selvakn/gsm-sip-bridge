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

## Batch 5 — complete the media contract (not started)

- [ ] RTP-01 — no RTCP sent or received, while its bandwidth is declared
- [ ] RTP-03 — pass-through relay forwards telephone-event under the wrong PT
- [ ] RTP-04 — no SSRC continuity check on receive
- [ ] SDP-06 — `a=rtcp` and the offer's `ptime` are discarded

## Batch 6 — the long tail (not started)

- [ ] MT-04 — `100rel` advertised but not served as a UAS
- [ ] MT-06 — preconditions are not implemented
- [ ] MT-11 — no `P-Access-Network-Info` on responses; SUBSCRIBE hardcodes
      Wi-Fi
- [ ] MT-12 — caller identity read from `From` alone, not
      `P-Asserted-Identity`/`Privacy`
- [ ] MT-13 — echoed `Via` gains no `received`/`rport`
- [ ] SDP-04 — an INVITE without an offer is rejected instead of answered
      with our own offer
- [ ] SDP-05 — multipart bodies are parsed by accident
- [ ] SMS-02 — TP-MTI never checked; every TPDU read as SMS-DELIVER
- [ ] SMS-03 — no RP-ERROR path
- [ ] SMS-04 — message-waiting/message-class DCS groups misread
- [ ] SMS-05 — concatenated messages labelled, not reassembled
- [ ] SMS-07 — national-language shift tables unimplemented
- [ ] CS-03 — no `AT+CNMI` policy asserted
- [ ] CS-04 — `+CMGR` header split on commas (not quote-aware)

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
