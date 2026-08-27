# Implementation Plan: Honour what the far side actually offered in SDP

**Branch**: `043-honour-sdp-negotiation` | **Date**: 2026-08-27 | **Spec**: [spec.md](spec.md)
**Input**: Feature specification from `/specs/043-honour-sdp-negotiation/spec.md`

## Summary

Batch 4 of the terminating-side protocol conformance review
(`docs/plans/mt-conformance-findings.md`): fix SDP-01 (extra media
sections silently dropped instead of declined), SDP-02 (direction
attributes ignored, answer always claims two-way), SDP-03 (transport
profile never checked), and confirm MT-05 (session timers) is already
resolved by prior batches. All three SDP findings are fixed by making
`gsm-sip-bridge/src/ims/sdp.rs`'s offer parser track every `m=` section it
sees (not just overwrite the last audio one), and making the answer
builder honestly describe what happens to each of them — without adding
any new media-relay capability. No change to the RTP relay itself; this
bridge remains a single-audio-stream, plain-RTP relay by design.

## Technical Context

**Language/Version**: Rust (existing crate edition/toolchain, unchanged)
**Primary Dependencies**: None new — pure additions to `crate::ims::sdp`'s existing parser/builder
**Storage**: N/A (SDP parsing is stateless per-call)
**Testing**: `cargo test` / `make test` (colocated `#[cfg(test)] mod tests` in `sdp.rs`, matching its existing const-fixture-plus-`sdp.contains(...)` style); real-hardware verification via `test/` docker rig + `siptest`, per Integration-First Testing
**Target Platform**: Linux (unchanged)
**Project Type**: Single Rust crate (existing structure, no new crates/binaries)
**Performance Goals**: N/A — correctness fix on an already-O(1)-per-call parse path
**Constraints**: Must not change single-audio-stream relay behavior or add real multi-stream/direction-gated relaying (explicitly out of scope, see spec Assumptions); must pass `make format && make lint && make test` (whole workspace, `-D warnings`) before any commit, per `CLAUDE.md`
**Scale/Scope**: 3 findings (SDP-01/02/03) fixed in one file plus its call site; 1 finding (MT-05) confirmed resolved with a test only; 0 new dependencies

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-checked after Phase 1 design — no changes, still passes.*

- **I. Integration-First Testing (NON-NEGOTIABLE)**: Every new function
  (`parse_offer`'s multi-section walk, the direction-mirroring helper, the
  transport-profile check) is exercised by real `SdpOffer`/answer-string
  values built the same way `sdp.rs`'s existing tests already do — no
  mocked parsing. Nothing here needs a live socket/session harness (unlike
  batch 3's retransmit-resend branches): SDP parsing and answer-building
  are pure functions today and remain so. **PASS.**
- **II. Green-on-Commit (NON-NEGOTIABLE)**: `make format && make lint &&
  make test` runs before each commit, whole workspace. **PASS** (gate
  applied at implementation time).
- **III. Frequent Atomic Commits**: Task breakdown groups by finding
  (SDP-01/multi-section, SDP-02/direction, SDP-03/transport-profile,
  MT-05/confirmation-test), matching batches 1-3's per-finding commit
  pattern. **PASS.**
- **IV. Makefile-Driven Build**: No new build steps or tooling. **PASS.**
- **V. Simplicity & Refactorability**: Central design constraint — see
  `research.md` Decision 1. Explicitly rejected adding real multi-stream
  relay support or direction-gated media suppression; the fix is answer
  *honesty*, not new relay capability, keeping the single-audio-stream
  design intact. **PASS.**

No violations. Complexity Tracking table is empty.

## Project Structure

### Documentation (this feature)

```text
specs/043-honour-sdp-negotiation/
├── plan.md              # This file
├── research.md          # Phase 0 output
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/
│   └── sdp-answer-contract.md
└── tasks.md              # Phase 2 output (/speckit.tasks)
```

### Source Code (repository root)

Single project (existing crate) — no new directories. Real paths, verified
against current source in Phase 0:

```text
gsm-sip-bridge/
├── src/ims/
│   ├── sdp.rs           # SdpOffer's new fields, parse_offer's multi-section
│   │                    # walk, build_answer_for's decline lines + direction
│   │                    # mirroring + transport-profile check — plus tests
│   └── agent/
│       └── inbound.rs   # No code change expected — SDP-03's decline reuses
│                        # the existing parse_offer-error → 488 path already
│                        # wired here; one confirmation test added instead
├── docs/plans/mt-conformance-findings.md  # mark SDP-01/02/03/MT-05 [x]
└── RELEASE_NOTES.md      # one entry under ## Unreleased
```

**Structure Decision**: No new modules or files. Everything lands inside
`src/ims/sdp.rs`, following that file's own established pattern (small
pure functions/enums colocated with the structs they extend, tests in the
existing `#[cfg(test)] mod tests` at the bottom) — the same pattern
batches 1-3 already used elsewhere in this codebase.

## Complexity Tracking

*(empty — no constitution violations)*
