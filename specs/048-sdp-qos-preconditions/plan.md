# Implementation Plan: Honour locally-confirmable SDP QoS preconditions

**Branch**: `048-sdp-qos-preconditions` | **Date**: 2026-08-28 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/048-sdp-qos-preconditions/spec.md`

## Summary

MT-06 from the terminating-side protocol conformance review
(`docs/plans/mt-conformance-findings.md`), deferred in batch 6 as needing
"a whole new subsystem." Turns out narrower once RFC 3312's segmented
model is applied precisely: this bridge's own segment (the offer's
`remote` status type, inverted to `local` in the answer — RFC 3312 §4,
confirmed against the actual RFC text) has no real reservation delay, so
it can be confirmed inline, synchronously, in the same `200 OK` this
bridge already sends — no `UPDATE`, no `100rel`, no state machine. What
genuinely cannot be honoured (an `e2e`-`mandatory` precondition, which
requires learning the *offerer's* segment status) is still declined, now
with the more specific `580 Precondition Failure` instead of the
current blanket `420`. The fix lands in three places: `SUPPORTED_EXTENSIONS`
gains `"precondition"` (moving the accept/decline decision from the header
alone to the SDP content), `sdp.rs` gains parsing for `a=des:qos`/
`a=curr:qos`/`a=conf:qos` plus the answer lines they produce, and
`agent/inbound.rs` gains one new decline branch parallel to SDP-03's
existing transport-profile check.

## Technical Context

**Language/Version**: Rust (existing crate edition/toolchain, unchanged)
**Primary Dependencies**: None new — pure additions to `crate::ims::sdp` and `crate::ims::agent`
**Storage**: N/A (precondition verdict is computed per-call, not persisted)
**Testing**: `cargo test` / `make test` (colocated `#[cfg(test)] mod tests` in `sdp.rs` and `agent/mod.rs`/`agent/inbound.rs`, matching each file's existing fixture style); real-hardware verification is regression-only (Clarifications) via `test/` docker rig + a real inbound call, per Integration-First Testing
**Target Platform**: Linux (unchanged)
**Project Type**: Single Rust crate (existing structure, no new crates/binaries)
**Performance Goals**: N/A — correctness fix on an already-O(1)-per-call parse path
**Constraints**: Must not add `UPDATE` method handling, `100rel`, or any multi-message readiness state machine (spec FR-009 — explicitly out of scope); must pass `make format && make lint && make test` (whole workspace, `-D warnings`) before any commit, per `CLAUDE.md`
**Scale/Scope**: 1 finding (MT-06) resolved; 3 files touched (`agent/mod.rs`'s `SUPPORTED_EXTENSIONS`, `sdp.rs`'s parser/answer-builder, `agent/inbound.rs`'s new decline branch); 1 new response builder (`build_580_precondition_failure`); 0 new dependencies

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design — no changes, still passes.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: Every new function
  (precondition-line parsing, verdict computation, answer-line building) is
  exercised by real `SdpOffer`/answer-string values built the same way
  `sdp.rs`'s existing tests already do — no mocked parsing. The one
  genuinely unreachable-live path (the new code can never be exercised by
  a real carrier call — research.md/spec.md Clarifications) is the
  documented, justified exception the constitution itself allows ("Mocks
  are permitted ONLY for external services that are impractical to run
  locally" — the closest analogue here is a carrier behavior that has
  never been and cannot be observed live, not a mock in the test code
  itself; every test remains a real, unmocked exercise of real functions).
  **PASS.**
- **II. Green-on-Commit (NON-NEGOTIABLE)**: `make format && make lint &&
  make test` runs before each commit, whole workspace. **PASS** (gate
  applied at implementation time).
- **III. Frequent Atomic Commits**: Task breakdown groups by concern
  (header-gate change, SDP parsing, verdict/answer-building, the new
  decline branch and response builder), matching prior batches' per-finding
  commit pattern even though this is a single finding. **PASS.**
- **IV. Makefile-Driven Build**: No new build steps or tooling. **PASS.**
- **V. Simplicity & Refactorability**: Central design constraint — see
  `research.md` Decisions 2 and 4. Explicitly rejected building `UPDATE`/
  `100rel`/a readiness state machine for the `e2e` case; the fix stays
  synchronous and pure, matching every prior batch's scope-boundary
  precedent (MT-02/MT-07/SDP-01/02/03). **PASS.**

No violations. Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/048-sdp-qos-preconditions/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md         # Phase 1 output
├── quickstart.md         # Phase 1 output
├── contracts/
│   └── precondition-answer-contract.md
└── tasks.md              # Phase 2 output (/speckit.tasks)
```

### Source Code (repository root)

Single project (existing crate) — no new directories. Real paths, verified
against current source in Phase 0:

```text
gsm-sip-bridge/
├── src/ims/
│   ├── sdp.rs           # QosStatusType/QosStrength/QosDirection/QosDesired/
│   │                    # QosStatus/PreconditionVerdict/QosAnswerLine; the
│   │                    # a=des:qos/a=curr:qos parsing added to parse_offer;
│   │                    # the a=curr/a=conf lines build_answer_for appends
│   │                    # — plus tests
│   ├── sip_client.rs    # New build_580_precondition_failure response
│   │                    # builder, alongside build_420_bad_extension/
│   │                    # build_488_*'s existing header-only decline shape
│   └── agent/
│       ├── mod.rs       # SUPPORTED_EXTENSIONS gains "precondition"; a
│       │                # confirming test that it's no longer listed
│       │                # Unsupported alone
│       └── inbound.rs   # New precondition-verdict check in handle_invite,
│                        # parallel to the existing SDP-03 transport check;
│                        # a confirming test for offerless-INVITE
│                        # non-interaction (Decision 5)
├── docs/plans/mt-conformance-findings.md  # mark MT-06 [x]
└── RELEASE_NOTES.md      # one entry under ## Unreleased
```

**Structure Decision**: No new modules or files. New types and functions
land inside `src/ims/sdp.rs` (parsing/answer-building) and
`src/ims/sip_client.rs` (the one new response builder), following each
file's own established pattern (small pure enums/structs colocated with
what they extend, tests in the existing `#[cfg(test)] mod tests` at the
bottom) — the same pattern batches 1-8 already used throughout this
codebase. `agent/mod.rs` and `agent/inbound.rs` get the smallest possible
changes: one list entry, one new branch reusing the existing decline
early-return shape.

## Complexity Tracking

*(empty — no constitution violations)*
