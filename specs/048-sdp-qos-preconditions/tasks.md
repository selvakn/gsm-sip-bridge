---

description: "Task list for MT-06: honour locally-confirmable SDP QoS preconditions"
---

# Tasks: Honour locally-confirmable SDP QoS preconditions

**Input**: Design documents from `/specs/048-sdp-qos-preconditions/`
**Prerequisites**: plan.md, spec.md, research.md, data-model.md, contracts/

**Tests**: Included — every new function here is pure and colocated with
existing `#[cfg(test)]` coverage in the same files, matching this
codebase's established Integration-First Testing convention. The one
exception (documented, not skipped) is that the new decline/accept
branches inside `handle_invite` cannot be exercised by a live carrier call
(spec Clarifications) — covered by fixture-driven unit tests instead, the
same posture already used for `handle_invite`'s other untestable-live
branches.

## Phase 1: Foundational (blocking prerequisite for all three user stories)

- [x] T001 In `gsm-sip-bridge/src/ims/sdp.rs`: add `QosStatusType`
      (`E2e`/`Local`/`Remote`), `QosStrength`
      (`Mandatory`/`Optional`/`None`/`Failure`/`Unknown`), `QosDirection`
      (`None`/`Send`/`Recv`/`SendRecv`), `QosDesired` (`strength`,
      `status_type`, `direction`), and `QosStatus` (`status_type`, `met`)
      per `data-model.md`. Extend `SdpOffer` with
      `preconditions: Vec<QosDesired>` and
      `offerer_curr: Vec<QosStatus>`. All status types are stored
      **offer-relative, unswapped** — inversion happens only where the
      answer is built (research.md Decision 1).
- [x] T002 In `gsm-sip-bridge/src/ims/sdp.rs`'s `parse_offer`: parse every
      `a=des:qos <strength> <status-type> <direction>` line in the
      selected audio section into `preconditions`, and every
      `a=curr:qos <status-type> <direction>` line into `offerer_curr`, in
      original order. An unrecognized strength/status-type/direction
      token falls back to `Unknown`/is skipped rather than failing the
      parse (research.md Decision 6, same permissive posture as `proto`).
      `parse_offer` must not newly fail on any offer that parsed
      successfully before this change.
- [x] T003 Fix the `Ok(SdpOffer { ... })` construction in `parse_offer` and
      any other exhaustive `SdpOffer { .. }` literals in `sdp.rs`'s test
      module for the two new fields.

**Checkpoint**: `SdpOffer` fully describes the offer's precondition
content; nothing yet changes what the answer says or whether the call is
accepted.

---

## Phase 2: User Story 1 - This bridge's own segment is confirmed instead of refused (Priority: P1)

**Goal**: An offer's `remote`-status-type `a=des:qos` line (this bridge's
own segment, once inverted — research.md Decision 1) is answered with
accurate `a=curr`/`a=conf` lines reporting it met, instead of the whole
call being refused.

**Independent Test**: Offer with `a=des:qos mandatory remote sendrecv` and
`Require: precondition` → call is not declined for the extension; answer's
SDP contains `a=curr:qos local sendrecv` + `a=conf:qos local sendrecv`.

- [x] T004 [US1] In `gsm-sip-bridge/src/ims/sdp.rs`: add
      `PreconditionVerdict` (`Proceed(Vec<QosAnswerLine>)` / `Decline`) and
      `QosAnswerLine`, plus a `precondition_verdict(&SdpOffer) ->
      PreconditionVerdict` function implementing the table in
      `data-model.md`: every `remote`-status-type `a=des:qos` line
      (any strength) produces a `local`-tagged `a=curr:qos sendrecv`
      answer line, plus a `local`-tagged `a=conf:qos` line when the
      line's strength is `mandatory` or `optional`.
- [x] T005 [US1] In `gsm-sip-bridge/src/ims/sdp.rs`'s `build_answer_for`:
      thread the offer's `precondition_verdict`'s `Proceed` answer lines
      through, appending each as its own `a=curr:qos`/`a=conf:qos\r\n`
      line after the existing `m=audio`/direction lines.
- [x] T006 [P] [US1] Tests in `sdp.rs`'s `tests` module: a `remote`
      `mandatory` line produces both `a=curr`/`a=conf`; a `remote`
      `optional` line produces both; a `remote` line with
      `none`/`failure`/`unknown` strength produces `a=curr` only (no
      `a=conf` — that strength doesn't ask for confirmation); an offer
      with `Require: precondition` but zero `a=des:qos` lines produces no
      new answer lines and is not declined (FR-007); two `remote` lines on
      the same status type with different directions (`sendrecv` and
      `recvonly`) each get their own accurate answer line rather than
      being merged.

**Checkpoint**: An offer whose only precondition content is this bridge's
own segment is answered honestly and proceeds; nothing yet handles `e2e`
or the offerer's own segment.

---

## Phase 3: User Story 2 - A genuinely unconfirmable `e2e` precondition is still declined (Priority: P1)

**Goal**: `Require: precondition` no longer triggers a blanket refusal on
its own, but an `e2e`-`mandatory` line — which this bridge cannot honestly
confirm — still declines the call, now with the more specific `580
Precondition Failure`.

**Independent Test**: Offer with `a=des:qos mandatory e2e sendrecv` and
`Require: precondition` → `580 Precondition Failure`, no answer body.
Same offer with `optional` instead of `mandatory` → call proceeds.

- [x] T007 [US2] In `gsm-sip-bridge/src/ims/sip_client.rs`: add
      `build_580_precondition_failure(request, to_tag) -> String`,
      mirroring `build_420_bad_extension`/`build_488_not_acceptable`'s
      header-only decline shape (RFC 3312 §6.2 — no body required).
- [x] T008 [US2] In `gsm-sip-bridge/src/ims/agent/mod.rs`: add
      `"precondition"` to `SUPPORTED_EXTENSIONS`. Update its doc comment's
      enumeration of placeholder-only tags to note `precondition` is now a
      real (bounded) capability, not a placeholder.
- [x] T009 [US2] In `gsm-sip-bridge/src/ims/sdp.rs`'s
      `precondition_verdict`: an `e2e`-status-type line at `mandatory`
      strength makes the whole verdict `Decline`, regardless of what other
      lines are present. An `e2e` line at any other strength contributes a
      `QosAnswerLine` reporting `e2e` status from this bridge's own
      segment alone (never claiming the offerer's contribution — research.md
      Decision 2/data-model.md's table).
- [x] T010 [US2] In `gsm-sip-bridge/src/ims/agent/inbound.rs`'s
      `handle_invite`: immediately after the existing `offer.proto ==
      "RTP/AVP"` check and before the codec-selection precheck, call
      `sdp::precondition_verdict(&offer)`; on `Decline`, send
      `build_580_precondition_failure`, report
      `CallStatus::Failed`/`BridgeFailureReason::BridgeSetupFailed`, and
      return `Ok(None)` — same shape as the existing transport/codec
      declines. On `Proceed`, carry the answer lines through to wherever
      `build_answer`/`build_veth_answer` is eventually called for this
      call.
- [x] T011 [P] [US2] Tests: `agent/mod.rs` — `Require: precondition` alone
      is no longer in `unsupported_required_extensions`'s output, while
      `Require: 100rel, precondition` still lists only `100rel` as
      unsupported. `sdp.rs` — `precondition_verdict` returns `Decline` for
      a lone `e2e`/`mandatory` line and for a combined `remote`/`mandatory`
      + `e2e`/`mandatory` offer (the unconfirmable line governs); returns
      `Proceed` for `e2e`/`optional`. `sip_client.rs` — the new builder
      produces `580 Precondition Failure` with no body. `inbound.rs` — an
      offer combining `mandatory e2e` with an otherwise-valid codec/transport
      reaches `580` rather than falling through to the codec check
      (confirms decline-check ordering: transport → precondition → codec).

**Checkpoint**: All of MT-06's accept/decline logic is in place. Only the
offerer's-own-segment mirroring (User Story 3) and polish remain.

---

## Phase 4: User Story 3 - The offerer's own segment is mirrored, never asserted (Priority: P3)

**Goal**: A `local`-status-type line in the offer (the caller's own
segment) never gets an invented confirmation from this bridge — if the
offer itself reported a current status for it, that status is mirrored
through inverted (`local`→`remote`) in the answer, unaltered.

**Independent Test**: Offer with only `a=des:qos mandatory local sendrecv`
plus a matching `a=curr:qos local none` → answer's SDP contains
`a=curr:qos remote none` (mirrored, not invented); call proceeds.

- [x] T012 [US3] In `gsm-sip-bridge/src/ims/sdp.rs`'s
      `precondition_verdict`: for each `local`-status-type entry in
      `offer.preconditions`, if `offer.offerer_curr` has a matching
      `local`-status-type `a=curr:qos` line, emit it through as a
      `remote`-tagged `QosAnswerLine` with the *same* `met` value the
      offer stated — never a value this bridge computes. If no matching
      `a=curr:qos local` line exists in the offer, emit nothing for that
      line (no invented default either).
- [x] T013 [P] [US3] Tests in `sdp.rs`: an offer with a `local`-status-type
      `a=des:qos` line and a matching `a=curr:qos local none` produces
      `a=curr:qos remote none` in the answer, byte-identical to the
      offer's own claim; an offer with a `local`-status-type `a=des:qos`
      line but **no** matching `a=curr:qos local` line produces no
      `remote`-tagged answer line at all (not a fabricated `none`).

**Checkpoint**: All three user stories land. Every row in
`contracts/precondition-answer-contract.md` is now implementable end to
end.

---

## Phase 5: Polish & Cross-Cutting

- [x] T014 [P] In `gsm-sip-bridge/src/ims/agent/inbound.rs`'s test module:
      add a confirming test that an offerless INVITE (empty body) carrying
      `Require: precondition` still proceeds through
      `handle_offerless_invite` unaffected — no `a=des:qos` lines exist to
      act on, so it takes the same path as today (research.md Decision 5,
      spec Edge Cases).
- [x] T015 Update `docs/plans/mt-conformance-findings.md`: mark MT-06
      `[x]` (moving it out of the "Deferred, not landed" list, matching
      how SDP-04/SMS-05 were moved out in batch 8's writeup), with a
      "Landed" description citing the RFC 3312 §4 local/remote-inversion
      correction, the `580` response code choice (§6.2), and the
      regression-only hardware-verification note.
- [x] T016 Add one entry to `RELEASE_NOTES.md` under `## Unreleased`
      describing the user-facing change: an inbound call requiring SDP QoS
      preconditions on its own segment now connects instead of being
      refused; a call requiring end-to-end preconditions this bridge
      cannot confirm is still declined, now with `580 Precondition
      Failure` instead of `420 Bad Extension`.
- [x] T017 `make format && make lint && make test` (whole workspace,
      clippy `-D warnings`) — must be clean before any commit, per
      `CLAUDE.md`.
- [ ] T018 Hardware round on the `test/` docker rig per `quickstart.md`
      (**regression-only**, per spec Clarifications): rebuild/retag,
      redeploy to the real Pi, re-register the real line, drive one
      ordinary real inbound call (no preconditions involved) and confirm
      no regression — call answers, audio both ways, clean hangup. Use
      `/discord-notify` to ask the user to place a call and wait for their
      confirmation before treating this as done. Record in the tracking
      doc that the precondition-specific logic itself remains unit-tested
      only, since no carrier reachable here can trigger it.

---

## Dependencies & Execution Order

- Phase 1 (Foundational) blocks all of Phase 2-4 — every user story reads
  or writes `SdpOffer`'s new fields.
- Phase 2 (US1) and Phase 4 (US3) are independent of each other (different
  status types, non-overlapping branches of `precondition_verdict`) but
  both build on Phase 1; Phase 3 (US2) needs `precondition_verdict` to
  already exist (from Phase 2) before adding its `Decline` branch, and
  needs the header-gate change (T008) done before its own decline path is
  reachable at all. Implement in priority order (US1 → US2 → US3) to match
  the spec's priorities and land as sequential commits per Constitution
  Principle III, rather than in parallel.
- Phase 5 depends on Phases 1-4 being complete.

## Implementation Strategy

Sequential, one user story per commit (matching prior batches' own commit
pattern): Foundational → US1 (confirm own segment) → US2 (header gate +
`580` decline for `e2e`/mandatory) → US3 (mirror offerer's own segment) →
offerless-INVITE confirmation test → docs → release notes → gate →
regression-only hardware round → PR.
