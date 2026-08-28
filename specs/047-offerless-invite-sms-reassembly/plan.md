# Implementation Plan: Offerless Call Answering and Multi-Part SMS Reassembly

**Branch**: `047-offerless-invite-sms-reassembly` | **Date**: 2026-08-28 | **Spec**: [spec.md](./spec.md)
**Input**: Feature specification from `/specs/047-offerless-invite-sms-reassembly/spec.md`

## Summary

Close **SDP-04** (an offerless inbound `INVITE` is rejected instead of
answered with this bridge's own offer) and **SMS-05** (a multi-part text
message is delivered as separate labelled fragments instead of one
reassembled message), both deferred out of batch 6 as "comparable in scope
to RTP-01."

Phase 0 found both are materially smaller than that estimate, because both
needed state a *different* existing path in this codebase already has:

- SDP-04 reuses `sdp::build_offer`/`sdp::parse_answer` verbatim — code
  `agent::origination.rs` already exercises on every outbound call this
  bridge places — and reuses the exact drain-loop shape
  `await_pbx_answer` already uses to service the carrier's signaling while
  blocked on something else. The actual new code is the branch, wait, and
  BYE-on-timeout wiring inside `handle_invite` itself.
- SMS-05 reuses the cross-route `Arc<Mutex<_>>`-sharing shape `Dedupe`
  already established between the IMS `MESSAGE` route and the modem-sweep
  route (CS-02), and reuses the modem-sweep thread's existing 20-second
  wakeup as the reassembly buffer's expiry clock instead of a new timer —
  the same "the existing wakeup already doubles as the clock" reasoning
  RTCP (batch 7) used for its own report cadence. The one genuine
  prerequisite is a small parser fix: the concatenation UDH's reference
  value is already read and then discarded; reassembly needs it kept.

One deliberate, documented scope cut, in the same spirit as RTP-01's
FR-023a residue: an offerless call's own offer carries audio codecs only —
no `telephone-event` (DTMF), no RTCP — because `build_offer` never has,
on the origination path it already serves, and extending it is real new
work with no live-observed need yet. Tracked as **FR-002a**.

## Technical Context

**Language/Version**: Rust (2021 edition), workspace at `gsm-sip-bridge/`
**Primary Dependencies**: none new.
**Storage**: N/A — both new pieces of state (`Reassembly`, the offerless
wait) are in-process only, discarded on restart, matching this codebase's
existing posture for `Dedupe` and every in-progress call.
**Testing**: `cargo test` via `make test`; unit tests in-module per this
codebase's convention.
**Target Platform**: Linux (aarch64 on the deployment Pi, x86-64 locally).
**Project Type**: Single Rust workspace — a long-running daemon plus CLIs.
**Performance Goals**: No new thread, no new socket. The offerless path
adds one bounded drain-loop wait inside an existing request handler; the
reassembly path adds one `HashMap` lookup per received SMS part and one
bounded scan per existing 20-second sweep pass.
**Constraints**: `make lint` is clippy `-D warnings` across the whole
workspace including test targets (CLAUDE.md); the ordinary (offer-present
INVITE, single-part SMS) paths must be provably unchanged (SC-005).
**Scale/Scope**: One call at a time per line (unchanged architecture); a
small, bounded number of concurrently in-flight multi-part messages per
line — this is a single-subscriber bridge, not a multi-tenant SMSC.

### Two decisions Phase 0 resolved that reduce scope

See `research.md` for the full list (10 decisions); the two that most
change what needs building:

- **Decision 2**: no new SDP-building/parsing code for SDP-04 —
  `build_offer`/`parse_answer` already exist and are already tested via
  the origination path. `origination.rs`'s private `offered_chosen_codec`
  is promoted to `pub(crate)` in `ims::sdp` and shared rather than
  duplicated.
- **Decision 6**: SMS-05's one real prerequisite is fixing
  `parse_concatenation_udh` to return the reference value it already
  parses and currently throws away — not new parsing, a bug in existing
  parsing that this feature's correctness requirement (FR-011) happens to
  expose.

No `NEEDS CLARIFICATION` remains.

## Constitution Check

*GATE: passed before Phase 0; re-checked after Phase 1 design — see below.*

| Principle | Assessment |
| --- | --- |
| **I. Integration-First Testing** (NON-NEGOTIABLE) | **Pass.** No mocks. `Reassembly::admit_part`/`take_expired` and the UDH reference fix are pure and directly tested, mirroring `Dedupe`'s own testing shape. SDP-04's wait/BYE-on-timeout branch is exercised through the real `handle_invite` structure with a synthetic empty-body request, the same way existing tests exercise its other declines (SDP-03, SDP-05). The one thing genuinely untestable without a live offerless-sending peer (none observed by this project to date) is disclosed in `quickstart.md` as unit-test-only this round, not silently assumed covered — consistent with every prior batch's treatment of a scenario no carrier here has been observed producing. |
| **II. Green-on-Commit** (NON-NEGOTIABLE) | **Pass.** `make format && make lint && make test` before every commit. |
| **III. Frequent Atomic Commits** | **Pass.** Phase breakdown below is written as commit boundaries. |
| **IV. Makefile-Driven Build** | **Pass.** No new tooling. |
| **V. Simplicity & Refactorability** | **Pass.** Both findings actively avoid new mechanisms where an existing one already fits: no new SDP functions (reuse), no new thread/timer (reuse the sweep thread), no new `ControlMessage` variant (reuse `SmsReceived`), no new drain-loop shape (reuse `await_pbx_answer`'s). The one new type, `Reassembly`, is not premature — it is what SMS-05 actually is — and is built as a near-structural twin of the already-proven `Dedupe`, not a novel design. |

**No Complexity Tracking entries** — no violation requires justification.

### Post-Phase 1 re-check

Re-evaluated after `data-model.md`. Still passing. One refinement the data
model surfaced: `Reassembly`'s `Complete` outcome must **not** clear its
buffer entry until the resulting forward actually succeeds — mirroring
`Dedupe::confirm`/`forget`'s existing retry-safety shape exactly, so a
downstream-send failure after successful reassembly doesn't require every
part to be retransmitted, only the one the network will actually retry.
This is a direct consequence of an existing pattern, not a new one —
recorded as a design detail, not a Constitution concern.

## Project Structure

### Documentation (this feature)

```text
specs/047-offerless-invite-sms-reassembly/
├── plan.md              # This file
├── spec.md              # Feature spec (1 clarification integrated)
├── research.md           # Phase 0 — 10 decisions
├── data-model.md         # Phase 1 — entities, validation, relationships
├── quickstart.md         # Phase 1 — verification plan
├── checklists/
│   └── requirements.md   # Spec quality checklist
└── tasks.md               # Phase 2 — NOT created by /speckit-plan
```

No `contracts/` directory: SDP-04 reuses the existing offer/answer wire
shape `ims::sdp` already documents and tests; SMS-05 changes no network-
facing contract at all (the network still sees the identical `200 OK`/
RP-ACK per part it does today — Decision 10). Unlike RTCP (batch 7), there
is no new wire protocol here worth a standalone contract file.

### Source Code (repository root)

```text
gsm-sip-bridge/
├── src/
│   ├── ims/
│   │   ├── sdp.rs               # + pub(crate) offered_chosen_codec (promoted from origination.rs)
│   │   ├── sms_pdu.rs            # parse_concatenation_udh + DecodedSms.part: new ConcatPart shape
│   │   └── agent/
│   │       ├── inbound.rs        # the offerless branch: recognize, offer, wait-for-ACK, BYE-on-timeout
│   │       ├── origination.rs    # offered_chosen_codec call site updated to the shared fn; no behavior change
│   │       └── mod.rs             # handle_message: reassembly hook between Dedupe and the SmsReceived send
│   └── volte/
│       └── sms.rs                 # + Reassembly, PartialMessage, PartOutcome; run_modem_reader: + expiry-flush pass
└── tests/                          # workspace integration tests, extended in place where relevant

docs/plans/mt-conformance-findings.md   # SDP-04 / SMS-05 status at the end
```

**Structure Decision**: No new modules. `Reassembly` lives beside `Dedupe`
in `volte::sms` because it is architecturally identical in shape and
shares its sharing/threading pattern exactly. `offered_chosen_codec` moves
from `origination.rs` (private) to `sdp.rs` (`pub(crate)`) because two
call sites now need the exact mapping it encodes, and it's `build_offer`'s
own contract, not `origination.rs`'s.

## Implementation Phases

Ordered as commit boundaries. Each compiles and tests green alone.

**Phase A — SMS-05 prerequisite: the UDH reference fix.** Change
`parse_concatenation_udh`'s return shape to carry the reference value
(`ConcatPart { reference, sequence, total }`), thread it through
`decode_user_data`/`DecodedSms.part`, update every existing call site and
test that matched the old `(u8, u8)` tuple. No new behavior yet — a real
single-part message still gets `part: None`; a concatenated one now
carries a field nothing reads yet. Smallest, most mechanical phase, and
the one with the highest "did I miss a call site" risk, so it lands and is
green alone before anything depends on it.

**Phase B — SMS-05: `Reassembly`.** The new type in `volte::sms`:
`admit_part`, `take_expired`, capacity/eviction mirroring `Dedupe`. Pure,
fully unit-testable, touches nothing else yet.

**Phase C — SMS-05: wire it into both routes.** `agent::mod`'s setup
constructs one `Arc<Mutex<Reassembly>>` alongside `dedupe` and threads it
into `InboundParams` and `run_modem_reader`'s parameters, same shape as
`dedupe`'s own threading. `handle_message`'s existing dedupe→relay
sequence gains the `admit_part` branch (Decision 9); `run_modem_reader`'s
sweep loop gains the `take_expired` flush pass (Decision 8). This is the
phase where SC-003/SC-004 become real end-to-end behavior.

**Phase D — SDP-04: recognize offerless, build our own offer.** In
`handle_invite`: the empty-body branch (Decision 1), `sdp::build_offer`
call with `ims_rtp_port` (bound the same place/way the existing path binds
it), `veth_wideband` sourced from `ctx.wideband` (Decision 5), the `200
OK` carrying our offer instead of an answer. Ends here, not yet waiting
for the ACK — this phase alone should leave the offerless path sending a
plausible offer and then falling through to whatever the *next* phase
adds, so it's reviewable as "here is the offer we'd send" before the
harder wait/teardown logic lands on top.

**Phase E — SDP-04: wait for the ACK, connect, relay; timeout/incompatible
teardown.** The bounded drain-loop wait on `inbound.rx` (Decision 3),
`sdp::parse_answer` on the ACK body, `sdp::offered_chosen_codec` (promoted
in this phase, per the Structure Decision above) to get a `ChosenCodec`,
the RTP connect + relay spawn + `ActiveCall` construction shared with the
normal path, and the BYE-via-`DialogInfo`/`CallEnded` teardown on timeout
or an incompatible answer (Decision 4). This is where US1's acceptance
scenarios all become real, and the largest single phase — it is the
"comparable to RTP-01" part the deferral note was pointing at.

**Phase F — `docs/plans/mt-conformance-findings.md` update.** Record
SDP-04/SMS-05 as landed, including FR-002a's DTMF/RTCP residue for the
offerless path (mirroring FR-023a's own entry) and whatever `quickstart.md`
step actually gets exercised (or doesn't) on real hardware this round.

**Sequencing note.** Phases A–C (SMS-05) and D–E (SDP-04) touch disjoint
files end-to-end (`volte/sms.rs` + the `handle_message` hook vs.
`inbound.rs` + `sdp.rs`'s promoted helper) and share no state — either
pair could land first with no effect on the other. A–C is ordered first
here only because Phase A's prerequisite fix is the phase most likely to
need a second pass if a call site is missed, and finding that out early
costs less than finding it after the larger SDP-04 phases are also
in flight.

## Risks

| Risk | Mitigation |
| --- | --- |
| The offerless-ACK wait's drain loop accidentally consumes/discards a request meant for something else running concurrently | It runs only inside `handle_invite`, the same window `await_pbx_answer`'s existing drain loop already owns exclusively — confirmed single-threaded around `inbound.rx` in `LoopState`. No new concurrency surface. |
| A live carrier's offerless INVITE (if one is ever actually observed) turns out to need DTMF/RTCP that FR-002a defers | Explicitly disclosed in the plan and in `mt-conformance-findings.md`, not silently assumed complete — the same posture FR-023a already set as precedent for this exact kind of residue. |
| The `Reassembly` capacity/eviction bound is set too low for a burst of legitimate concurrent multi-part messages, or too high to bound memory meaningfully | Mirrors `Dedupe`'s existing bound (64), already sized for this single-subscriber bridge's realistic traffic; revisit only if a live round shows it binding. |
| Missing a call site in Phase A's `DecodedSms.part` shape change | `make lint` (clippy `-D warnings`, whole workspace including tests) turns a missed match arm into a build failure, not a silent behavior change — Phase A is deliberately its own commit specifically so this surfaces immediately. |
| SDP-04 cannot be exercised on real hardware this round (no observed offerless-sending peer) | Disclosed in `quickstart.md`, not assumed — consistent with prior batches' treatment of their own unexercised paths. |

## Complexity Tracking

No Constitution Check violations. Table intentionally empty.
